//! Graph-backed commit message authorization.
//!
//! The application hook computes deterministic facts for `git commit`: amend
//! status, message presence, conventional format, project-scoped Linear
//! reference requirements, and the derived allow/block decision. This graph
//! authorizes those facts through durable LangGraph checkpoints before the CLI
//! permits a commit command.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::commit_message_validator::CommitMessageEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CommitMessageDecision {
    #[default]
    Unclassified,
    Allow,
    AllowAmend,
    AllowNoMessage,
    AllowConventional,
    BlockMalformed,
    BlockMissingLinearRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitMessageState {
    pub identifier: String,
    pub tool: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub command_sha256: Option<String>,
    pub git_commit: bool,
    pub amend: bool,
    pub message_present: bool,
    pub message_sha256: Option<String>,
    pub conventional: bool,
    pub effective_cwd_present: bool,
    pub project_present: bool,
    pub linear_prefix_count: u64,
    pub linear_ref_required: bool,
    pub linear_ref_present: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: CommitMessageDecision,
}

impl CommitMessageState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &CommitMessageEvaluation,
    ) -> Self {
        let command_sha256 = evaluation
            .command
            .as_deref()
            .filter(|_| evaluation.command_present)
            .map(sha256);
        let message_sha256 = evaluation.message.as_deref().map(sha256);
        let blocking_finding_count = u64::from(expected_should_block(
            evaluation.bash_tool,
            evaluation.git_commit,
            evaluation.amend,
            evaluation.message.is_some(),
            evaluation.conventional,
            evaluation.linear_ref_required,
            evaluation.linear_ref_present,
        ));
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            bash_tool: evaluation.bash_tool,
            command_present: evaluation.command_present,
            command_sha256,
            git_commit: evaluation.git_commit,
            amend: evaluation.amend,
            message_present: evaluation.message.is_some(),
            message_sha256,
            conventional: evaluation.conventional,
            effective_cwd_present: evaluation.effective_cwd.is_some(),
            project_present: evaluation.project.is_some(),
            linear_prefix_count: evaluation.linear_prefixes.len() as u64,
            linear_ref_required: evaluation.linear_ref_required,
            linear_ref_present: evaluation.linear_ref_present,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: CommitMessageDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitMessageGraphRun {
    pub state: CommitMessageState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<CommitMessageState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct CommitMessageAuthorization {
    decision: CommitMessageDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl CommitMessageAuthorization {
    #[must_use]
    pub fn decision(&self) -> CommitMessageDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl CommitMessageGraphRun {
    #[must_use]
    pub fn commit_message_authorization(
        &self,
    ) -> Result<Option<CommitMessageAuthorization>, String> {
        if self.state.decision == CommitMessageDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "commit_message",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(CommitMessageAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const ALLOW_AMEND: &str = "allow_amend";
const ALLOW_NO_MESSAGE: &str = "allow_no_message";
const ALLOW_CONVENTIONAL: &str = "allow_conventional";
const BLOCK_MALFORMED: &str = "block_malformed";
const BLOCK_MISSING_LINEAR_REF: &str = "block_missing_linear_ref";

pub type CommitMessageGraph = CompilationResult<CommitMessageState>;

#[must_use]
pub fn commit_message_decision_label(decision: CommitMessageDecision) -> &'static str {
    match decision {
        CommitMessageDecision::Unclassified => "unclassified",
        CommitMessageDecision::Allow => "allow",
        CommitMessageDecision::AllowAmend => "allow-amend",
        CommitMessageDecision::AllowNoMessage => "allow-no-message",
        CommitMessageDecision::AllowConventional => "allow-conventional",
        CommitMessageDecision::BlockMalformed => "block-malformed",
        CommitMessageDecision::BlockMissingLinearRef => "block-missing-linear-ref",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn expected_should_block(
    bash_tool: bool,
    git_commit: bool,
    amend: bool,
    message_present: bool,
    conventional: bool,
    linear_ref_required: bool,
    linear_ref_present: bool,
) -> bool {
    bash_tool
        && git_commit
        && !amend
        && message_present
        && (!conventional || (linear_ref_required && !linear_ref_present))
}

fn expected_decision(state: &CommitMessageState) -> CommitMessageDecision {
    if !state.bash_tool || !state.git_commit {
        CommitMessageDecision::Allow
    } else if state.amend {
        CommitMessageDecision::AllowAmend
    } else if !state.message_present {
        CommitMessageDecision::AllowNoMessage
    } else if !state.conventional {
        CommitMessageDecision::BlockMalformed
    } else if state.linear_ref_required && !state.linear_ref_present {
        CommitMessageDecision::BlockMissingLinearRef
    } else {
        CommitMessageDecision::AllowConventional
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "commit_message")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn commit_message_state_schema() -> StateSchema<CommitMessageState> {
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
                "git_commit",
                "amend",
                "message_present",
                "message_sha256",
                "conventional",
                "effective_cwd_present",
                "project_present",
                "linear_prefix_count",
                "linear_ref_required",
                "linear_ref_present",
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
                "git_commit": { "type": "boolean" },
                "amend": { "type": "boolean" },
                "message_present": { "type": "boolean" },
                "message_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "conventional": { "type": "boolean" },
                "effective_cwd_present": { "type": "boolean" },
                "project_present": { "type": "boolean" },
                "linear_prefix_count": { "type": "integer", "minimum": 0 },
                "linear_ref_required": { "type": "boolean" },
                "linear_ref_present": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "AllowAmend",
                        "AllowNoMessage",
                        "AllowConventional",
                        "BlockMalformed",
                        "BlockMissingLinearRef"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "commit_message",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &CommitMessageState| {
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
                    "commit_message bash_tool requires Bash, got {tool}"
                )));
            }
            if !state.command_present {
                if state.command_sha256.is_some()
                    || state.git_commit
                    || state.amend
                    || state.message_present
                    || state.message_sha256.is_some()
                    || state.conventional
                    || state.project_present
                    || state.linear_ref_required
                    || state.linear_ref_present
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "commit_message missing-command state cannot carry commit facts"
                            .to_string(),
                    ));
                }
            } else if !optional_hex_digest_present(&state.command_sha256) {
                return Err(StateError::ValidationFailed(
                    "commit_message command_sha256 must be a 64-character hex digest".to_string(),
                ));
            }
            if !state.bash_tool
                && (state.git_commit
                    || state.amend
                    || state.message_present
                    || state.conventional
                    || state.project_present
                    || state.linear_ref_required
                    || state.linear_ref_present
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "commit_message non-Bash state cannot authorize commit messages".to_string(),
                ));
            }
            if !state.git_commit
                && (state.amend
                    || state.message_present
                    || state.conventional
                    || state.project_present
                    || state.linear_ref_required
                    || state.linear_ref_present
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "commit_message non-commit state cannot carry commit facts".to_string(),
                ));
            }
            if state.amend
                && (state.message_present
                    || state.conventional
                    || state.project_present
                    || state.linear_ref_required
                    || state.linear_ref_present
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "commit_message amend commits bypass message validation".to_string(),
                ));
            }
            if state.message_present {
                if !optional_hex_digest_present(&state.message_sha256) {
                    return Err(StateError::ValidationFailed(
                        "commit_message message_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.message_sha256.is_some()
                || state.conventional
                || state.project_present
                || state.linear_ref_required
                || state.linear_ref_present
            {
                return Err(StateError::ValidationFailed(
                    "commit_message missing-message state cannot carry message/project facts"
                        .to_string(),
                ));
            }
            if state.project_present && !state.effective_cwd_present {
                return Err(StateError::ValidationFailed(
                    "commit_message project detection requires an effective cwd".to_string(),
                ));
            }
            if state.linear_ref_required {
                if !state.project_present || state.linear_prefix_count == 0 {
                    return Err(StateError::ValidationFailed(
                        "commit_message linear ref requirement requires project prefixes"
                            .to_string(),
                    ));
                }
            } else if state.linear_prefix_count > 0 || state.linear_ref_present {
                return Err(StateError::ValidationFailed(
                    "commit_message linear facts require a linear ref requirement".to_string(),
                ));
            }
            if state.linear_ref_present && !state.linear_ref_required {
                return Err(StateError::ValidationFailed(
                    "commit_message linear_ref_present requires linear_ref_required".to_string(),
                ));
            }
            let expected_should_block = expected_should_block(
                state.bash_tool,
                state.git_commit,
                state.amend,
                state.message_present,
                state.conventional,
                state.linear_ref_required,
                state.linear_ref_present,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "commit_message should_block must match commit message policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "commit_message blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != CommitMessageDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "commit_message terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: CommitMessageState) -> Result<CommitMessageState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_commit_message_graph() -> Result<CommitMessageGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("commit_message").await?;
    build_commit_message_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_commit_message_graph_with_ephemeral_sqlite() -> Result<CommitMessageGraph, String> {
    build_commit_message_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_commit_message_graph_with_database_path(
    db_path: &str,
) -> Result<CommitMessageGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_commit_message_graph_with_checkpointer(checkpointer).await
}

async fn build_commit_message_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<CommitMessageGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = commit_message_state_schema();
    let builder = StateGraphBuilder::<CommitMessageState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: CommitMessageState| async move {
                emit_decision_node_event("commit_message", CLASSIFY, &s.identifier)?;
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
            |s: CommitMessageState| async move {
                emit_decision_node_event("commit_message", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = CommitMessageDecision::Allow;
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
            ALLOW_AMEND,
            |s: CommitMessageState| async move {
                emit_decision_node_event("commit_message", ALLOW_AMEND, &s.identifier)?;
                let mut next = s;
                next.decision = CommitMessageDecision::AllowAmend;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_AMEND,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW_NO_MESSAGE,
            |s: CommitMessageState| async move {
                emit_decision_node_event("commit_message", ALLOW_NO_MESSAGE, &s.identifier)?;
                let mut next = s;
                next.decision = CommitMessageDecision::AllowNoMessage;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_NO_MESSAGE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW_CONVENTIONAL,
            |s: CommitMessageState| async move {
                emit_decision_node_event("commit_message", ALLOW_CONVENTIONAL, &s.identifier)?;
                let mut next = s;
                next.decision = CommitMessageDecision::AllowConventional;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_CONVENTIONAL,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK_MALFORMED,
            |s: CommitMessageState| async move {
                emit_decision_node_event("commit_message", BLOCK_MALFORMED, &s.identifier)?;
                let mut next = s;
                next.decision = CommitMessageDecision::BlockMalformed;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_MALFORMED,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK_MISSING_LINEAR_REF,
            |s: CommitMessageState| async move {
                emit_decision_node_event(
                    "commit_message",
                    BLOCK_MISSING_LINEAR_REF,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = CommitMessageDecision::BlockMissingLinearRef;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_MISSING_LINEAR_REF,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &CommitMessageState| {
            match expected_decision(s) {
                CommitMessageDecision::Allow => ALLOW.into(),
                CommitMessageDecision::AllowAmend => ALLOW_AMEND.into(),
                CommitMessageDecision::AllowNoMessage => ALLOW_NO_MESSAGE.into(),
                CommitMessageDecision::AllowConventional => ALLOW_CONVENTIONAL.into(),
                CommitMessageDecision::BlockMalformed => BLOCK_MALFORMED.into(),
                CommitMessageDecision::BlockMissingLinearRef => BLOCK_MISSING_LINEAR_REF.into(),
                CommitMessageDecision::Unclassified => ALLOW.into(),
            }
        })
        .add_edge(ALLOW, END)
        .add_edge(ALLOW_AMEND, END)
        .add_edge(ALLOW_NO_MESSAGE, END)
        .add_edge(ALLOW_CONVENTIONAL, END)
        .add_edge(BLOCK_MALFORMED, END)
        .add_edge(BLOCK_MISSING_LINEAR_REF, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_commit_message_decision_report(
    compiled: &CommitMessageGraph,
    state: CommitMessageState,
) -> Result<CommitMessageGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "commit_message",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "commit_message", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "commit_message",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(CommitMessageGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: commit_message_graph_topology(compiled)?,
    })
}

pub fn commit_message_graph_topology(
    compiled: &CommitMessageGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("commit_message", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::commit_message_validator::{
        CommitMessageDecision as AppDecision, CommitMessageEvaluation,
    };

    fn evaluation(message: Option<&str>) -> CommitMessageEvaluation {
        CommitMessageEvaluation {
            tool: Some("Bash".to_string()),
            command: Some(match message {
                Some(message) => format!("git commit -m '{message}'"),
                None => "git commit".to_string(),
            }),
            bash_tool: true,
            command_present: true,
            git_commit: true,
            amend: false,
            message: message.map(str::to_string),
            conventional: message.is_some_and(|message| message.starts_with("feat:")),
            effective_cwd: None,
            project: None,
            linear_prefixes: Vec::new(),
            linear_ref_required: false,
            linear_ref_present: false,
            should_block: message.is_some_and(|message| !message.starts_with("feat:")),
            decision: if message.is_none() {
                AppDecision::AllowNoMessage
            } else if message.is_some_and(|message| message.starts_with("feat:")) {
                AppDecision::AllowConventional
            } else {
                AppDecision::BlockMalformed
            },
        }
    }

    #[tokio::test]
    async fn graph_authorizes_malformed_block() {
        let graph = build_commit_message_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = CommitMessageState::from_evaluation(
            "commit-message-block",
            &evaluation(Some("updated the thing")),
        );
        let run = run_commit_message_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, CommitMessageDecision::BlockMalformed);
        assert!(run
            .commit_message_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_conventional_allow() {
        let graph = build_commit_message_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = CommitMessageState::from_evaluation(
            "commit-message-allow",
            &evaluation(Some("feat: add workflow")),
        );
        let run = run_commit_message_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, CommitMessageDecision::AllowConventional);
    }

    #[tokio::test]
    async fn graph_authorizes_commit_without_message_with_absent_message_hash() {
        let graph = build_commit_message_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            CommitMessageState::from_evaluation("commit-message-no-message", &evaluation(None));
        assert_eq!(state.message_sha256, None);

        let run = run_commit_message_decision_report(&graph, state)
            .await
            .unwrap();

        assert_eq!(run.state.decision, CommitMessageDecision::AllowNoMessage);
        assert_eq!(run.state.message_sha256, None);
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_command_without_command_digest() {
        let graph = build_commit_message_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(Some("feat: add workflow"));
        eval.command = None;
        eval.command_present = true;
        let state =
            CommitMessageState::from_evaluation("commit-message-missing-command-digest", &eval);

        let err = run_commit_message_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("command_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_message_with_extra_message_digest() {
        let graph = build_commit_message_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = CommitMessageState::from_evaluation(
            "commit-message-extra-message-hash",
            &evaluation(None),
        );
        state.message_sha256 = Some(sha256("ghost message"));

        let err = run_commit_message_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("missing-message"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_linear_allow() {
        let graph = build_commit_message_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(Some("feat: add workflow"));
        eval.project = Some("firefly".to_string());
        eval.effective_cwd = Some("/repo/firefly".to_string());
        eval.linear_prefixes = vec!["FPCRM".to_string()];
        eval.linear_ref_required = true;
        eval.linear_ref_present = false;
        eval.should_block = false;
        eval.decision = AppDecision::AllowConventional;
        let mut state = CommitMessageState::from_evaluation("commit-message-forged", &eval);
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_commit_message_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }
}
