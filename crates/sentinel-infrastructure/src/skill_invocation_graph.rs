//! Graph-backed skill invocation authorization.
//!
//! The application hook records pending skill state when the skill router
//! detects a mandatory skill. This graph authorizes the PreToolUse decision for
//! live pending skill markers: allow fix-path tools and subagents, or block
//! regular tools until the detected skill is invoked.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::skill_invocation_gate::SkillInvocationEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SkillInvocationDecision {
    #[default]
    Unclassified,
    Allow,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInvocationState {
    pub identifier: String,
    pub tool: Option<String>,
    pub session_id_present: bool,
    pub session_id_sha256: Option<String>,
    pub subagent_call: bool,
    pub pending_skill_present: bool,
    pub pending_skill_stale: bool,
    pub pending_state_session_matches: bool,
    pub allowed_tool: bool,
    pub skill_present: bool,
    pub skill_sha256: Option<String>,
    pub skill_path_present: bool,
    pub detected_at_present: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: SkillInvocationDecision,
}

impl SkillInvocationState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &SkillInvocationEvaluation,
    ) -> Self {
        let session_id_sha256 = evaluation
            .session_id
            .as_deref()
            .filter(|_| evaluation.session_id_present)
            .map(sha256);
        let skill_sha256 = evaluation
            .skill
            .as_deref()
            .filter(|_| evaluation.skill_present)
            .map(sha256);
        let should_block = expected_should_block(
            evaluation.subagent_call,
            evaluation.session_id_present,
            evaluation.pending_skill_present,
            evaluation.pending_skill_stale,
            evaluation.allowed_tool,
        );
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            session_id_present: evaluation.session_id_present,
            session_id_sha256,
            subagent_call: evaluation.subagent_call,
            pending_skill_present: evaluation.pending_skill_present,
            pending_skill_stale: evaluation.pending_skill_stale,
            pending_state_session_matches: evaluation.pending_state_session_matches,
            allowed_tool: evaluation.allowed_tool,
            skill_present: evaluation.skill_present,
            skill_sha256,
            skill_path_present: evaluation.skill_path_present,
            detected_at_present: evaluation.detected_at_present,
            blocking_finding_count: u64::from(should_block),
            should_block,
            decision: SkillInvocationDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillInvocationGraphRun {
    pub state: SkillInvocationState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SkillInvocationState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct SkillInvocationAuthorization {
    decision: SkillInvocationDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl SkillInvocationAuthorization {
    #[must_use]
    pub fn decision(&self) -> SkillInvocationDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl SkillInvocationGraphRun {
    #[must_use]
    pub fn skill_invocation_authorization(
        &self,
    ) -> Result<Option<SkillInvocationAuthorization>, String> {
        if self.state.decision == SkillInvocationDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "skill_invocation",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SkillInvocationAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK: &str = "block";

pub type SkillInvocationGraph = CompilationResult<SkillInvocationState>;

#[must_use]
pub fn skill_invocation_decision_label(decision: SkillInvocationDecision) -> &'static str {
    match decision {
        SkillInvocationDecision::Unclassified => "unclassified",
        SkillInvocationDecision::Allow => "allow",
        SkillInvocationDecision::Block => "block",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn expected_should_block(
    subagent_call: bool,
    session_id_present: bool,
    pending_skill_present: bool,
    pending_skill_stale: bool,
    allowed_tool: bool,
) -> bool {
    !subagent_call
        && session_id_present
        && pending_skill_present
        && !pending_skill_stale
        && !allowed_tool
}

fn expected_decision(state: &SkillInvocationState) -> SkillInvocationDecision {
    if state.should_block {
        SkillInvocationDecision::Block
    } else {
        SkillInvocationDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "skill_invocation")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn skill_invocation_state_schema() -> StateSchema<SkillInvocationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "session_id_present",
                "session_id_sha256",
                "subagent_call",
                "pending_skill_present",
                "pending_skill_stale",
                "pending_state_session_matches",
                "allowed_tool",
                "skill_present",
                "skill_sha256",
                "skill_path_present",
                "detected_at_present",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tool": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
                        { "type": "null" }
                    ]
                },
                "session_id_present": { "type": "boolean" },
                "session_id_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "subagent_call": { "type": "boolean" },
                "pending_skill_present": { "type": "boolean" },
                "pending_skill_stale": { "type": "boolean" },
                "pending_state_session_matches": { "type": "boolean" },
                "allowed_tool": { "type": "boolean" },
                "skill_present": { "type": "boolean" },
                "skill_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "skill_path_present": { "type": "boolean" },
                "detected_at_present": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "skill_invocation",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SkillInvocationState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if state.session_id_present {
                if !optional_hex_digest_present(&state.session_id_sha256) {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation session_id_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.session_id_sha256.is_some()
                || state.pending_skill_present
                || state.pending_skill_stale
                || state.pending_state_session_matches
                || state.skill_present
                || state.skill_sha256.is_some()
                || state.skill_path_present
                || state.detected_at_present
                || state.blocking_finding_count > 0
                || state.should_block
            {
                return Err(StateError::ValidationFailed(
                    "skill_invocation missing session cannot carry pending skill facts"
                        .to_string(),
                ));
            }

            if state.subagent_call {
                if state.pending_skill_present
                    || state.pending_skill_stale
                    || state.pending_state_session_matches
                    || state.skill_present
                    || state.skill_sha256.is_some()
                    || state.skill_path_present
                    || state.detected_at_present
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation subagent state cannot carry main-session pending skill facts"
                            .to_string(),
                    ));
                }
            }

            if !state.pending_skill_present {
                if state.pending_skill_stale
                    || state.pending_state_session_matches
                    || state.skill_present
                    || state.skill_sha256.is_some()
                    || state.skill_path_present
                    || state.detected_at_present
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation no-pending state cannot carry pending skill facts"
                            .to_string(),
                    ));
                }
            } else {
                if !state.session_id_present {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation pending skill requires session id evidence"
                            .to_string(),
                    ));
                }
                if !state.pending_state_session_matches {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation pending state session_id must match current session"
                            .to_string(),
                    ));
                }
                if !state.skill_present || !state.skill_path_present || !state.detected_at_present
                {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation pending skill requires skill, skill_path, and detected_at"
                            .to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.skill_sha256) {
                    return Err(StateError::ValidationFailed(
                        "skill_invocation skill_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            }

            let expected_should_block = expected_should_block(
                state.subagent_call,
                state.session_id_present,
                state.pending_skill_present,
                state.pending_skill_stale,
                state.allowed_tool,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "skill_invocation should_block must match pending skill policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }

            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "skill_invocation blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }

            if state.decision != SkillInvocationDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "skill_invocation terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }

            Ok(())
        })
}

async fn classify_node(state: SkillInvocationState) -> Result<SkillInvocationState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_skill_invocation_graph() -> Result<SkillInvocationGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("skill_invocation").await?;
    build_skill_invocation_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_skill_invocation_graph_with_ephemeral_sqlite() -> Result<SkillInvocationGraph, String>
{
    build_skill_invocation_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_skill_invocation_graph_with_database_path(
    db_path: &str,
) -> Result<SkillInvocationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_skill_invocation_graph_with_checkpointer(checkpointer).await
}

async fn build_skill_invocation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SkillInvocationGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = skill_invocation_state_schema();
    let builder = StateGraphBuilder::<SkillInvocationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: SkillInvocationState| async move {
                emit_decision_node_event("skill_invocation", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW,
            |s: SkillInvocationState| async move {
                emit_decision_node_event("skill_invocation", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = SkillInvocationDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK,
            |s: SkillInvocationState| async move {
                emit_decision_node_event("skill_invocation", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = SkillInvocationDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &SkillInvocationState| match expected_decision(s) {
                SkillInvocationDecision::Allow => ALLOW.into(),
                SkillInvocationDecision::Block => BLOCK.into(),
                SkillInvocationDecision::Unclassified => ALLOW.into(),
            },
        )
        .add_edge(ALLOW, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_skill_invocation_decision_report(
    compiled: &SkillInvocationGraph,
    state: SkillInvocationState,
) -> Result<SkillInvocationGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "skill_invocation",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "skill_invocation", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "skill_invocation",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SkillInvocationGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: skill_invocation_graph_topology(compiled)?,
    })
}

pub fn skill_invocation_graph_topology(
    compiled: &SkillInvocationGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("skill_invocation", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::skill_invocation_gate::{
        SkillInvocationDecision as AppDecision, SkillInvocationEvaluation,
    };

    fn evaluation(
        tool: &str,
        allowed_tool: bool,
        decision: AppDecision,
    ) -> SkillInvocationEvaluation {
        SkillInvocationEvaluation {
            tool: Some(tool.to_string()),
            session_id: Some("session-1".to_string()),
            subagent_call: false,
            session_id_present: true,
            pending_skill_present: true,
            pending_skill_stale: false,
            pending_state_session_matches: true,
            allowed_tool,
            skill: Some("linear".to_string()),
            skill_present: true,
            skill_path_present: true,
            detected_at_present: true,
            should_block: matches!(decision, AppDecision::Block),
            decision,
        }
    }

    fn no_pending_evaluation() -> SkillInvocationEvaluation {
        SkillInvocationEvaluation {
            tool: Some("Bash".to_string()),
            session_id: Some("session-1".to_string()),
            subagent_call: false,
            session_id_present: true,
            pending_skill_present: false,
            pending_skill_stale: false,
            pending_state_session_matches: false,
            allowed_tool: false,
            skill: None,
            skill_present: false,
            skill_path_present: false,
            detected_at_present: false,
            should_block: false,
            decision: AppDecision::Allow,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_pending_skill_block() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SkillInvocationState::from_evaluation(
            "skill-invocation-block",
            &evaluation("Bash", false, AppDecision::Block),
        );
        assert!(state.session_id_sha256.is_some());
        assert!(state.skill_sha256.is_some());
        let run = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SkillInvocationDecision::Block);
        assert!(run
            .skill_invocation_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_allowed_tool_allow() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SkillInvocationState::from_evaluation(
            "skill-invocation-allowlisted",
            &evaluation("Skill", true, AppDecision::Allow),
        );
        let run = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SkillInvocationDecision::Allow);
    }

    #[tokio::test]
    async fn graph_authorizes_stale_pending_skill_allow() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation("Bash", false, AppDecision::Allow);
        eval.pending_skill_stale = true;
        let state = SkillInvocationState::from_evaluation("skill-invocation-stale", &eval);
        let run = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SkillInvocationDecision::Allow);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_for_pending_skill() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = SkillInvocationState::from_evaluation(
            "skill-invocation-forged",
            &evaluation("Bash", false, AppDecision::Block),
        );
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_mismatched_pending_session() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = SkillInvocationState::from_evaluation(
            "skill-invocation-mismatched-session",
            &evaluation("Bash", false, AppDecision::Block),
        );
        state.pending_state_session_matches = false;
        let err = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("session_id must match"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_session_without_session_digest() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation("Bash", false, AppDecision::Block);
        eval.session_id = None;
        eval.session_id_present = true;
        let state =
            SkillInvocationState::from_evaluation("skill-invocation-missing-session-digest", &eval);

        let err = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("session_id_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_pending_skill_without_skill_digest() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation("Bash", false, AppDecision::Block);
        eval.skill = None;
        eval.skill_present = true;
        let state =
            SkillInvocationState::from_evaluation("skill-invocation-missing-skill-digest", &eval);

        let err = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("skill_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_no_pending_state_with_extra_skill_digest() {
        let graph = build_skill_invocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = SkillInvocationState::from_evaluation(
            "skill-invocation-extra-skill",
            &no_pending_evaluation(),
        );
        state.skill_sha256 = Some(sha256("ghost skill"));

        let err = run_skill_invocation_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("no-pending"), "{err}");
    }
}
