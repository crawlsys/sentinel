//! Graph-backed pre-commit/pre-push verification authorization.
//!
//! The application hook computes deterministic facts for `git commit` and
//! `git push`: action class, content-only/docs-only exemptions, signed
//! override status, and session-recorded verification evidence. This graph
//! authorizes the resulting allow/block decision through durable LangGraph
//! checkpoints before the CLI permits source-control mutation.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::pre_commit_verification::{
    PreCommitAction, PreCommitVerificationEvaluation,
};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreCommitVerificationDecision {
    #[default]
    Unclassified,
    Allow,
    AllowContentOnlyRepo,
    AllowDocsOnly,
    AllowSignedOverride,
    AllowRecordedEvidence,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreCommitVerificationState {
    pub identifier: String,
    pub tool: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub command_sha256: Option<String>,
    pub action: String,
    pub git_mutation: bool,
    pub content_only_repo: bool,
    pub docs_only_change: bool,
    pub signed_override_active: bool,
    pub recorded_evidence_present: bool,
    pub session_id_present: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: PreCommitVerificationDecision,
}

impl PreCommitVerificationState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &PreCommitVerificationEvaluation,
    ) -> Self {
        let command_sha256 = evaluation
            .command
            .as_deref()
            .filter(|_| evaluation.command_present)
            .map(command_sha256);
        let git_mutation = !matches!(evaluation.action, PreCommitAction::None);
        let blocking_finding_count = u64::from(expected_should_block(
            evaluation.bash_tool,
            git_mutation,
            evaluation.content_only_repo,
            evaluation.docs_only_change,
            evaluation.signed_override_active,
            evaluation.recorded_evidence_present,
        ));
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            bash_tool: evaluation.bash_tool,
            command_present: evaluation.command_present,
            command_sha256,
            action: action_label(evaluation.action).to_string(),
            git_mutation,
            content_only_repo: evaluation.content_only_repo,
            docs_only_change: evaluation.docs_only_change,
            signed_override_active: evaluation.signed_override_active,
            recorded_evidence_present: evaluation.recorded_evidence_present,
            session_id_present: evaluation.session_id_present,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: PreCommitVerificationDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PreCommitVerificationGraphRun {
    pub state: PreCommitVerificationState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<PreCommitVerificationState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct PreCommitVerificationAuthorization {
    decision: PreCommitVerificationDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl PreCommitVerificationAuthorization {
    #[must_use]
    pub fn decision(&self) -> PreCommitVerificationDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl PreCommitVerificationGraphRun {
    #[must_use]
    pub fn pre_commit_verification_authorization(
        &self,
    ) -> Result<Option<PreCommitVerificationAuthorization>, String> {
        if self.state.decision == PreCommitVerificationDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "pre_commit_verification",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(PreCommitVerificationAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const ALLOW_CONTENT_ONLY_REPO: &str = "allow_content_only_repo";
const ALLOW_DOCS_ONLY: &str = "allow_docs_only";
const ALLOW_SIGNED_OVERRIDE: &str = "allow_signed_override";
const ALLOW_RECORDED_EVIDENCE: &str = "allow_recorded_evidence";
const BLOCK: &str = "block";

pub type PreCommitVerificationGraph = CompilationResult<PreCommitVerificationState>;

#[must_use]
pub const fn action_label(action: PreCommitAction) -> &'static str {
    match action {
        PreCommitAction::None => "none",
        PreCommitAction::Commit => "commit",
        PreCommitAction::Push => "push",
    }
}

#[must_use]
pub fn pre_commit_verification_decision_label(
    decision: PreCommitVerificationDecision,
) -> &'static str {
    match decision {
        PreCommitVerificationDecision::Unclassified => "unclassified",
        PreCommitVerificationDecision::Allow => "allow",
        PreCommitVerificationDecision::AllowContentOnlyRepo => "allow-content-only-repo",
        PreCommitVerificationDecision::AllowDocsOnly => "allow-docs-only",
        PreCommitVerificationDecision::AllowSignedOverride => "allow-signed-override",
        PreCommitVerificationDecision::AllowRecordedEvidence => "allow-recorded-evidence",
        PreCommitVerificationDecision::Block => "block",
    }
}

#[must_use]
pub fn command_sha256(command: &str) -> String {
    hex::encode(Sha256::digest(command.as_bytes()))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn expected_should_block(
    bash_tool: bool,
    git_mutation: bool,
    content_only_repo: bool,
    docs_only_change: bool,
    signed_override_active: bool,
    recorded_evidence_present: bool,
) -> bool {
    bash_tool
        && git_mutation
        && !content_only_repo
        && !docs_only_change
        && !signed_override_active
        && !recorded_evidence_present
}

fn expected_decision(state: &PreCommitVerificationState) -> PreCommitVerificationDecision {
    if !state.bash_tool || !state.git_mutation {
        PreCommitVerificationDecision::Allow
    } else if state.content_only_repo {
        PreCommitVerificationDecision::AllowContentOnlyRepo
    } else if state.docs_only_change {
        PreCommitVerificationDecision::AllowDocsOnly
    } else if state.signed_override_active {
        PreCommitVerificationDecision::AllowSignedOverride
    } else if state.recorded_evidence_present {
        PreCommitVerificationDecision::AllowRecordedEvidence
    } else {
        PreCommitVerificationDecision::Block
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "pre_commit_verification")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn pre_commit_verification_state_schema() -> StateSchema<PreCommitVerificationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "bash_tool",
                "command_present",
                "command_sha256",
                "action",
                "git_mutation",
                "content_only_repo",
                "docs_only_change",
                "signed_override_active",
                "recorded_evidence_present",
                "session_id_present",
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
                "bash_tool": { "type": "boolean" },
                "command_present": { "type": "boolean" },
                "command_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "action": { "type": "string", "enum": ["none", "commit", "push"] },
                "git_mutation": { "type": "boolean" },
                "content_only_repo": { "type": "boolean" },
                "docs_only_change": { "type": "boolean" },
                "signed_override_active": { "type": "boolean" },
                "recorded_evidence_present": { "type": "boolean" },
                "session_id_present": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "AllowContentOnlyRepo",
                        "AllowDocsOnly",
                        "AllowSignedOverride",
                        "AllowRecordedEvidence",
                        "Block"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "pre_commit_verification",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &PreCommitVerificationState| {
            let Some(tool) = state
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
            else {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            };
            if state.bash_tool && tool != "Bash" {
                return Err(StateError::ValidationFailed(format!(
                    "pre_commit_verification bash_tool requires Bash, got {tool}"
                )));
            }
            if !state.command_present {
                if state.command_sha256.is_some()
                    || state.action != "none"
                    || state.git_mutation
                    || state.content_only_repo
                    || state.docs_only_change
                    || state.signed_override_active
                    || state.recorded_evidence_present
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "pre_commit_verification missing-command state cannot carry git mutation facts"
                            .to_string(),
                    ));
                }
            } else if !optional_hex_digest_present(&state.command_sha256) {
                return Err(StateError::ValidationFailed(
                    "pre_commit_verification command_sha256 must be a 64-character hex digest"
                        .to_string(),
                ));
            }
            let expected_git_mutation = matches!(state.action.as_str(), "commit" | "push");
            if state.git_mutation != expected_git_mutation {
                return Err(StateError::ValidationFailed(format!(
                    "pre_commit_verification git_mutation must match action: expected \
                     {expected_git_mutation}, got {}",
                    state.git_mutation
                )));
            }
            if !state.bash_tool
                && (state.git_mutation
                    || state.content_only_repo
                    || state.docs_only_change
                    || state.signed_override_active
                    || state.recorded_evidence_present
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "pre_commit_verification non-Bash state cannot authorize git mutations"
                        .to_string(),
                ));
            }
            if !state.git_mutation
                && (state.content_only_repo
                    || state.docs_only_change
                    || state.signed_override_active
                    || state.recorded_evidence_present
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "pre_commit_verification non-mutation state cannot carry verification facts"
                        .to_string(),
                ));
            }
            if state.content_only_repo
                && (state.docs_only_change
                    || state.signed_override_active
                    || state.recorded_evidence_present)
            {
                return Err(StateError::ValidationFailed(
                    "pre_commit_verification content-only repo is the first terminal allow"
                        .to_string(),
                ));
            }
            if state.docs_only_change
                && (state.signed_override_active || state.recorded_evidence_present)
            {
                return Err(StateError::ValidationFailed(
                    "pre_commit_verification docs-only change is the terminal allow before overrides/evidence"
                        .to_string(),
                ));
            }
            if state.signed_override_active && !state.session_id_present {
                return Err(StateError::ValidationFailed(
                    "pre_commit_verification signed override requires a session id".to_string(),
                ));
            }
            if state.recorded_evidence_present {
                if !state.session_id_present {
                    return Err(StateError::ValidationFailed(
                        "pre_commit_verification recorded evidence requires a session id"
                            .to_string(),
                    ));
                }
                if state.signed_override_active {
                    return Err(StateError::ValidationFailed(
                        "pre_commit_verification signed override wins before evidence"
                            .to_string(),
                    ));
                }
            }
            let expected_should_block = expected_should_block(
                state.bash_tool,
                state.git_mutation,
                state.content_only_repo,
                state.docs_only_change,
                state.signed_override_active,
                state.recorded_evidence_present,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "pre_commit_verification should_block must match verification policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "pre_commit_verification blocking_finding_count must match should_block: \
                     expected {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != PreCommitVerificationDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "pre_commit_verification terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(
    state: PreCommitVerificationState,
) -> Result<PreCommitVerificationState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_pre_commit_verification_graph() -> Result<PreCommitVerificationGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("pre_commit_verification").await?;
    build_pre_commit_verification_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_pre_commit_verification_graph_with_ephemeral_sqlite(
) -> Result<PreCommitVerificationGraph, String> {
    build_pre_commit_verification_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_pre_commit_verification_graph_with_database_path(
    db_path: &str,
) -> Result<PreCommitVerificationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_pre_commit_verification_graph_with_checkpointer(checkpointer).await
}

async fn build_pre_commit_verification_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<PreCommitVerificationGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = pre_commit_verification_state_schema();
    let builder = StateGraphBuilder::<PreCommitVerificationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event("pre_commit_verification", CLASSIFY, &s.identifier)?;
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
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event("pre_commit_verification", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = PreCommitVerificationDecision::Allow;
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
            ALLOW_CONTENT_ONLY_REPO,
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event(
                    "pre_commit_verification",
                    ALLOW_CONTENT_ONLY_REPO,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PreCommitVerificationDecision::AllowContentOnlyRepo;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_CONTENT_ONLY_REPO,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            ALLOW_DOCS_ONLY,
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event(
                    "pre_commit_verification",
                    ALLOW_DOCS_ONLY,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PreCommitVerificationDecision::AllowDocsOnly;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_DOCS_ONLY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            ALLOW_SIGNED_OVERRIDE,
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event(
                    "pre_commit_verification",
                    ALLOW_SIGNED_OVERRIDE,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PreCommitVerificationDecision::AllowSignedOverride;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_SIGNED_OVERRIDE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            ALLOW_RECORDED_EVIDENCE,
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event(
                    "pre_commit_verification",
                    ALLOW_RECORDED_EVIDENCE,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = PreCommitVerificationDecision::AllowRecordedEvidence;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_RECORDED_EVIDENCE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            BLOCK,
            |s: PreCommitVerificationState| async move {
                emit_decision_node_event("pre_commit_verification", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = PreCommitVerificationDecision::Block;
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
        .add_conditional_edge(
            CLASSIFY,
            |s: &PreCommitVerificationState| match expected_decision(s) {
                PreCommitVerificationDecision::Allow => ALLOW.into(),
                PreCommitVerificationDecision::AllowContentOnlyRepo => {
                    ALLOW_CONTENT_ONLY_REPO.into()
                }
                PreCommitVerificationDecision::AllowDocsOnly => ALLOW_DOCS_ONLY.into(),
                PreCommitVerificationDecision::AllowSignedOverride => ALLOW_SIGNED_OVERRIDE.into(),
                PreCommitVerificationDecision::AllowRecordedEvidence => {
                    ALLOW_RECORDED_EVIDENCE.into()
                }
                PreCommitVerificationDecision::Block => BLOCK.into(),
                PreCommitVerificationDecision::Unclassified => ALLOW.into(),
            },
        )
        .add_edge(ALLOW, END)
        .add_edge(ALLOW_CONTENT_ONLY_REPO, END)
        .add_edge(ALLOW_DOCS_ONLY, END)
        .add_edge(ALLOW_SIGNED_OVERRIDE, END)
        .add_edge(ALLOW_RECORDED_EVIDENCE, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_pre_commit_verification_decision_report(
    compiled: &PreCommitVerificationGraph,
    state: PreCommitVerificationState,
) -> Result<PreCommitVerificationGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "pre_commit_verification",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "pre_commit_verification",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "pre_commit_verification",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(PreCommitVerificationGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: pre_commit_verification_graph_topology(compiled)?,
    })
}

pub fn pre_commit_verification_graph_topology(
    compiled: &PreCommitVerificationGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("pre_commit_verification", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::pre_commit_verification::PreCommitDecision as AppDecision;

    fn evaluation(action: PreCommitAction) -> PreCommitVerificationEvaluation {
        PreCommitVerificationEvaluation {
            tool: Some("Bash".to_string()),
            command: Some(match action {
                PreCommitAction::None => "ls -la".to_string(),
                PreCommitAction::Commit => "git commit -m test".to_string(),
                PreCommitAction::Push => "git push origin main".to_string(),
            }),
            bash_tool: true,
            command_present: true,
            action,
            content_only_repo: false,
            docs_only_change: false,
            signed_override_active: false,
            recorded_evidence_present: false,
            session_id_present: true,
            should_block: !matches!(action, PreCommitAction::None),
            decision: if matches!(action, PreCommitAction::None) {
                AppDecision::Allow
            } else {
                AppDecision::Block
            },
        }
    }

    fn missing_command_evaluation() -> PreCommitVerificationEvaluation {
        PreCommitVerificationEvaluation {
            tool: Some("Bash".to_string()),
            command: None,
            bash_tool: true,
            command_present: false,
            action: PreCommitAction::None,
            content_only_repo: false,
            docs_only_change: false,
            signed_override_active: false,
            recorded_evidence_present: false,
            session_id_present: true,
            should_block: false,
            decision: AppDecision::Allow,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_commit_block_without_evidence() {
        let graph = build_pre_commit_verification_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = PreCommitVerificationState::from_evaluation(
            "commit-block",
            &evaluation(PreCommitAction::Commit),
        );
        assert!(state.command_sha256.is_some());
        let run = run_pre_commit_verification_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, PreCommitVerificationDecision::Block);
        assert!(run
            .pre_commit_verification_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_recorded_evidence_allow() {
        let graph = build_pre_commit_verification_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(PreCommitAction::Push);
        eval.recorded_evidence_present = true;
        eval.should_block = false;
        eval.decision = AppDecision::AllowRecordedEvidence;
        let state = PreCommitVerificationState::from_evaluation("push-evidence", &eval);
        let run = run_pre_commit_verification_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            PreCommitVerificationDecision::AllowRecordedEvidence
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_command_without_command_digest() {
        let graph = build_pre_commit_verification_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(PreCommitAction::Commit);
        eval.command = None;
        eval.command_present = true;
        let state = PreCommitVerificationState::from_evaluation("missing-command-digest", &eval);

        let err = run_pre_commit_verification_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("command_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_command_with_extra_command_digest() {
        let graph = build_pre_commit_verification_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = PreCommitVerificationState::from_evaluation(
            "extra-command-digest",
            &missing_command_evaluation(),
        );
        state.command_sha256 = Some(command_sha256("ghost git commit"));

        let err = run_pre_commit_verification_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("missing-command"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_evidence_without_session() {
        let graph = build_pre_commit_verification_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(PreCommitAction::Commit);
        eval.recorded_evidence_present = true;
        eval.session_id_present = false;
        eval.should_block = false;
        eval.decision = AppDecision::AllowRecordedEvidence;
        let state = PreCommitVerificationState::from_evaluation("forged-evidence", &eval);
        let err = run_pre_commit_verification_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(
            err.contains("recorded evidence requires a session id"),
            "{err}"
        );
    }
}
