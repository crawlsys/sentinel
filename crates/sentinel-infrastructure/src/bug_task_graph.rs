//! Graph-backed bug task authorization.
//!
//! The application hook records pending bug state when tool output reveals a
//! high-confidence failure. This graph authorizes the PreToolUse decision for
//! sessions with a live pending bug: allow fix-path tools, or block mutating work
//! until a bug task is filed. The authorization is checkpointed through
//! LangGraph before the CLI permits the tool call.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::bug_task_gate::BugTaskEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BugTaskDecision {
    #[default]
    Unclassified,
    Allow,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugTaskState {
    pub identifier: String,
    pub tool: Option<String>,
    pub repo_root_present: bool,
    pub repo_root_sha256: Option<String>,
    pub pending_bug_present: bool,
    pub pending_bug_stale: bool,
    pub pending_state_repo_root_matches: bool,
    pub allowed_tool: bool,
    pub evidence_present: bool,
    pub evidence_sha256: Option<String>,
    pub first_seen_at_present: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: BugTaskDecision,
}

impl BugTaskState {
    #[must_use]
    pub fn from_evaluation(identifier: impl Into<String>, evaluation: &BugTaskEvaluation) -> Self {
        let repo_root_sha256 = evaluation
            .repo_root
            .as_deref()
            .filter(|_| evaluation.repo_root_present)
            .map(sha256);
        let evidence_sha256 = evaluation
            .evidence
            .as_deref()
            .filter(|_| evaluation.evidence_present)
            .map(sha256);
        let should_block = expected_should_block(
            evaluation.repo_root_present,
            evaluation.pending_bug_present,
            evaluation.pending_bug_stale,
            evaluation.allowed_tool,
        );
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            repo_root_present: evaluation.repo_root_present,
            repo_root_sha256,
            pending_bug_present: evaluation.pending_bug_present,
            pending_bug_stale: evaluation.pending_bug_stale,
            pending_state_repo_root_matches: evaluation.pending_state_repo_root_matches,
            allowed_tool: evaluation.allowed_tool,
            evidence_present: evaluation.evidence_present,
            evidence_sha256,
            first_seen_at_present: evaluation.first_seen_at_present,
            blocking_finding_count: u64::from(should_block),
            should_block,
            decision: BugTaskDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BugTaskGraphRun {
    pub state: BugTaskState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<BugTaskState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct BugTaskAuthorization {
    decision: BugTaskDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl BugTaskAuthorization {
    #[must_use]
    pub fn decision(&self) -> BugTaskDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl BugTaskGraphRun {
    #[must_use]
    pub fn bug_task_authorization(&self) -> Result<Option<BugTaskAuthorization>, String> {
        if self.state.decision == BugTaskDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "bug_task",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(BugTaskAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK: &str = "block";

pub type BugTaskGraph = CompilationResult<BugTaskState>;

#[must_use]
pub fn bug_task_decision_label(decision: BugTaskDecision) -> &'static str {
    match decision {
        BugTaskDecision::Unclassified => "unclassified",
        BugTaskDecision::Allow => "allow",
        BugTaskDecision::Block => "block",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn expected_should_block(
    repo_root_present: bool,
    pending_bug_present: bool,
    pending_bug_stale: bool,
    allowed_tool: bool,
) -> bool {
    repo_root_present && pending_bug_present && !pending_bug_stale && !allowed_tool
}

fn expected_decision(state: &BugTaskState) -> BugTaskDecision {
    if state.should_block {
        BugTaskDecision::Block
    } else {
        BugTaskDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "bug_task")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn bug_task_state_schema() -> StateSchema<BugTaskState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "repo_root_present",
                "repo_root_sha256",
                "pending_bug_present",
                "pending_bug_stale",
                "pending_state_repo_root_matches",
                "allowed_tool",
                "evidence_present",
                "evidence_sha256",
                "first_seen_at_present",
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
                "repo_root_present": { "type": "boolean" },
                "repo_root_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "pending_bug_present": { "type": "boolean" },
                "pending_bug_stale": { "type": "boolean" },
                "pending_state_repo_root_matches": { "type": "boolean" },
                "allowed_tool": { "type": "boolean" },
                "evidence_present": { "type": "boolean" },
                "evidence_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "first_seen_at_present": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "bug_task",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &BugTaskState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if state.repo_root_present {
                if !optional_hex_digest_present(&state.repo_root_sha256) {
                    return Err(StateError::ValidationFailed(
                        "bug_task repo_root_sha256 must be a 64-character hex digest".to_string(),
                    ));
                }
            } else if state.repo_root_sha256.is_some()
                || state.pending_bug_present
                || state.pending_bug_stale
                || state.pending_state_repo_root_matches
                || state.evidence_present
                || state.evidence_sha256.is_some()
                || state.first_seen_at_present
                || state.blocking_finding_count > 0
                || state.should_block
            {
                return Err(StateError::ValidationFailed(
                    "bug_task missing repo state cannot carry pending bug facts".to_string(),
                ));
            }

            if !state.pending_bug_present {
                if state.pending_bug_stale
                    || state.pending_state_repo_root_matches
                    || state.evidence_present
                    || state.evidence_sha256.is_some()
                    || state.first_seen_at_present
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "bug_task no-pending state cannot carry pending bug facts".to_string(),
                    ));
                }
            } else {
                if !state.repo_root_present {
                    return Err(StateError::ValidationFailed(
                        "bug_task pending bug requires repo root evidence".to_string(),
                    ));
                }
                if !state.pending_state_repo_root_matches {
                    return Err(StateError::ValidationFailed(
                        "bug_task pending bug state repo_root must match current repo root"
                            .to_string(),
                    ));
                }
                if !state.evidence_present || !state.first_seen_at_present {
                    return Err(StateError::ValidationFailed(
                        "bug_task pending bug requires evidence and first_seen_at".to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.evidence_sha256) {
                    return Err(StateError::ValidationFailed(
                        "bug_task evidence_sha256 must be a 64-character hex digest".to_string(),
                    ));
                }
            }

            let expected_should_block = expected_should_block(
                state.repo_root_present,
                state.pending_bug_present,
                state.pending_bug_stale,
                state.allowed_tool,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "bug_task should_block must match pending bug policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }

            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "bug_task blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }

            if state.decision != BugTaskDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "bug_task terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }

            Ok(())
        })
}

async fn classify_node(state: BugTaskState) -> Result<BugTaskState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_bug_task_graph() -> Result<BugTaskGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("bug_task").await?;
    build_bug_task_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_bug_task_graph_with_ephemeral_sqlite() -> Result<BugTaskGraph, String> {
    build_bug_task_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_bug_task_graph_with_database_path(db_path: &str) -> Result<BugTaskGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_bug_task_graph_with_checkpointer(checkpointer).await
}

async fn build_bug_task_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<BugTaskGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = bug_task_state_schema();
    let builder = StateGraphBuilder::<BugTaskState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: BugTaskState| async move {
                emit_decision_node_event("bug_task", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: BugTaskState| async move {
                emit_decision_node_event("bug_task", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = BugTaskDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            BLOCK,
            |s: BugTaskState| async move {
                emit_decision_node_event("bug_task", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = BugTaskDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &BugTaskState| match expected_decision(s) {
            BugTaskDecision::Allow => ALLOW.into(),
            BugTaskDecision::Block => BLOCK.into(),
            BugTaskDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_bug_task_decision_report(
    compiled: &BugTaskGraph,
    state: BugTaskState,
) -> Result<BugTaskGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "bug_task",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "bug_task", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "bug_task",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(BugTaskGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: bug_task_graph_topology(compiled)?,
    })
}

pub fn bug_task_graph_topology(compiled: &BugTaskGraph) -> Result<DecisionGraphTopology, String> {
    topology("bug_task", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::bug_task_gate::{
        BugTaskDecision as AppDecision, BugTaskEvaluation,
    };

    fn evaluation(tool: &str, allowed_tool: bool, decision: AppDecision) -> BugTaskEvaluation {
        BugTaskEvaluation {
            tool: Some(tool.to_string()),
            cwd: Some("/repo".to_string()),
            repo_root: Some("/repo".to_string()),
            repo_root_present: true,
            pending_bug_present: true,
            pending_bug_stale: false,
            pending_state_repo_root_matches: true,
            allowed_tool,
            evidence: Some("test result: FAILED".to_string()),
            evidence_present: true,
            first_seen_at_present: true,
            should_block: matches!(decision, AppDecision::Block),
            decision,
        }
    }

    fn no_pending_evaluation() -> BugTaskEvaluation {
        BugTaskEvaluation {
            tool: Some("Bash".to_string()),
            cwd: Some("/repo".to_string()),
            repo_root: Some("/repo".to_string()),
            repo_root_present: true,
            pending_bug_present: false,
            pending_bug_stale: false,
            pending_state_repo_root_matches: false,
            allowed_tool: false,
            evidence: None,
            evidence_present: false,
            first_seen_at_present: false,
            should_block: false,
            decision: AppDecision::Allow,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_pending_bug_block() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let state = BugTaskState::from_evaluation(
            "bug-task-block",
            &evaluation("Bash", false, AppDecision::Block),
        );
        assert!(state.repo_root_sha256.is_some());
        assert!(state.evidence_sha256.is_some());
        let run = run_bug_task_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, BugTaskDecision::Block);
        assert!(run
            .bug_task_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_allowlisted_tool_allow() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let state = BugTaskState::from_evaluation(
            "bug-task-allowlisted",
            &evaluation("TaskCreate", true, AppDecision::Allow),
        );
        let run = run_bug_task_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, BugTaskDecision::Allow);
    }

    #[tokio::test]
    async fn graph_authorizes_stale_pending_bug_allow() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let mut eval = evaluation("Bash", false, AppDecision::Allow);
        eval.pending_bug_stale = true;
        let state = BugTaskState::from_evaluation("bug-task-stale", &eval);
        let run = run_bug_task_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, BugTaskDecision::Allow);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_for_pending_bug() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state = BugTaskState::from_evaluation(
            "bug-task-forged",
            &evaluation("Bash", false, AppDecision::Block),
        );
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_bug_task_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_mismatched_pending_repo_root() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state = BugTaskState::from_evaluation(
            "bug-task-mismatched-repo",
            &evaluation("Bash", false, AppDecision::Block),
        );
        state.pending_state_repo_root_matches = false;
        let err = run_bug_task_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("repo_root must match"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_repo_without_repo_digest() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let mut eval = evaluation("Bash", false, AppDecision::Block);
        eval.repo_root = None;
        eval.repo_root_present = true;
        let state = BugTaskState::from_evaluation("bug-task-missing-repo-digest", &eval);

        let err = run_bug_task_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("repo_root_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_pending_bug_without_evidence_digest() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let mut eval = evaluation("Bash", false, AppDecision::Block);
        eval.evidence = None;
        eval.evidence_present = true;
        let state = BugTaskState::from_evaluation("bug-task-missing-evidence-digest", &eval);

        let err = run_bug_task_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("evidence_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_no_pending_state_with_extra_evidence_digest() {
        let graph = build_bug_task_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state =
            BugTaskState::from_evaluation("bug-task-extra-evidence", &no_pending_evaluation());
        state.evidence_sha256 = Some(sha256("ghost evidence"));

        let err = run_bug_task_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("no-pending"), "{err}");
    }
}
