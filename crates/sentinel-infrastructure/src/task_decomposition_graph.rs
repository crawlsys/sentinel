//! Graph-backed task decomposition authorization.
//!
//! The application hook computes deterministic facts for mutating tool calls:
//! whether the tool can change state, whether a live task list can be confirmed
//! for the session, and whether Sentinel must block because decomposition
//! compliance is unproven. This graph authorizes the resulting allow/block
//! decision through durable LangGraph checkpoints before the CLI permits a
//! mutating tool call.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::task_decomposition_gate::TaskDecompositionEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TaskDecompositionDecision {
    #[default]
    Unclassified,
    Allow,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDecompositionState {
    pub identifier: String,
    pub tool: Option<String>,
    pub session_id_present: bool,
    pub session_id_sha256: Option<String>,
    pub allowed_tool: bool,
    pub bash_tool: bool,
    pub bash_command_present: bool,
    pub bash_command_sha256: Option<String>,
    pub mutating_tool: bool,
    pub task_state_readable: bool,
    pub task_list_confirmed: bool,
    pub unreadable_task_state: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: TaskDecompositionDecision,
}

impl TaskDecompositionState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &TaskDecompositionEvaluation,
    ) -> Self {
        let session_id_sha256 = evaluation
            .session_id
            .as_deref()
            .filter(|session_id| !session_id.is_empty())
            .map(sha256);
        let bash_command_sha256 = evaluation
            .bash_command
            .as_deref()
            .filter(|_| evaluation.bash_command_present)
            .map(sha256);
        let should_block =
            expected_should_block(evaluation.mutating_tool, evaluation.task_list_confirmed);
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            session_id_present: evaluation
                .session_id
                .as_deref()
                .is_some_and(|session_id| !session_id.is_empty()),
            session_id_sha256,
            allowed_tool: evaluation.allowed_tool,
            bash_tool: evaluation.bash_tool,
            bash_command_present: evaluation.bash_command_present,
            bash_command_sha256,
            mutating_tool: evaluation.mutating_tool,
            task_state_readable: evaluation.task_state_readable,
            task_list_confirmed: evaluation.task_list_confirmed,
            unreadable_task_state: evaluation.unreadable_task_state,
            blocking_finding_count: u64::from(should_block),
            should_block,
            decision: TaskDecompositionDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDecompositionGraphRun {
    pub state: TaskDecompositionState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<TaskDecompositionState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct TaskDecompositionAuthorization {
    decision: TaskDecompositionDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl TaskDecompositionAuthorization {
    #[must_use]
    pub fn decision(&self) -> TaskDecompositionDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl TaskDecompositionGraphRun {
    #[must_use]
    pub fn task_decomposition_authorization(
        &self,
    ) -> Result<Option<TaskDecompositionAuthorization>, String> {
        if self.state.decision == TaskDecompositionDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "task_decomposition",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(TaskDecompositionAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK: &str = "block";

pub type TaskDecompositionGraph = CompilationResult<TaskDecompositionState>;

#[must_use]
pub fn task_decomposition_decision_label(decision: TaskDecompositionDecision) -> &'static str {
    match decision {
        TaskDecompositionDecision::Unclassified => "unclassified",
        TaskDecompositionDecision::Allow => "allow",
        TaskDecompositionDecision::Block => "block",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn expected_should_block(mutating_tool: bool, task_list_confirmed: bool) -> bool {
    mutating_tool && !task_list_confirmed
}

fn expected_decision(state: &TaskDecompositionState) -> TaskDecompositionDecision {
    if state.should_block {
        TaskDecompositionDecision::Block
    } else {
        TaskDecompositionDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "task_decomposition")
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

fn task_decomposition_state_schema() -> StateSchema<TaskDecompositionState> {
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
                "allowed_tool",
                "bash_tool",
                "bash_command_present",
                "bash_command_sha256",
                "mutating_tool",
                "task_state_readable",
                "task_list_confirmed",
                "unreadable_task_state",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tool": {
                    "anyOf": [
                        { "type": "string" },
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
                "allowed_tool": { "type": "boolean" },
                "bash_tool": { "type": "boolean" },
                "bash_command_present": { "type": "boolean" },
                "bash_command_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "mutating_tool": { "type": "boolean" },
                "task_state_readable": { "type": "boolean" },
                "task_list_confirmed": { "type": "boolean" },
                "unreadable_task_state": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "task_decomposition",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &TaskDecompositionState| {
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
            if state.session_id_present {
                if !optional_hex_digest_present(&state.session_id_sha256) {
                    return Err(StateError::ValidationFailed(
                        "task_decomposition session_id_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.session_id_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "task_decomposition missing session id cannot carry a session hash".to_string(),
                ));
            }

            if state.bash_command_present {
                if !state.bash_tool {
                    return Err(StateError::ValidationFailed(
                        "task_decomposition bash command facts require Bash tool".to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.bash_command_sha256) {
                    return Err(StateError::ValidationFailed(
                        "task_decomposition bash_command_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.bash_command_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "task_decomposition missing Bash command cannot carry command hash".to_string(),
                ));
            }

            if state.bash_tool && tool != "Bash" {
                return Err(StateError::ValidationFailed(format!(
                    "task_decomposition bash_tool requires Bash tool, got {tool}"
                )));
            }

            if state.allowed_tool && state.mutating_tool {
                return Err(StateError::ValidationFailed(
                    "task_decomposition allowed fix-path tools cannot be mutating".to_string(),
                ));
            }

            if state.bash_tool && state.mutating_tool && !state.bash_command_present {
                return Err(StateError::ValidationFailed(
                    "task_decomposition mutating Bash requires an inspectable command".to_string(),
                ));
            }

            if !state.mutating_tool {
                if state.task_state_readable
                    || state.task_list_confirmed
                    || state.unreadable_task_state
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "task_decomposition non-mutating state cannot carry authority findings"
                            .to_string(),
                    ));
                }
            }

            if state.task_list_confirmed && !state.task_state_readable {
                return Err(StateError::ValidationFailed(
                    "task_decomposition confirmed task list requires readable task state"
                        .to_string(),
                ));
            }

            let expected_unreadable_task_state = state.mutating_tool && !state.task_state_readable;
            if state.unreadable_task_state != expected_unreadable_task_state {
                return Err(StateError::ValidationFailed(format!(
                    "task_decomposition unreadable_task_state must match readable task-state \
                     evidence: expected {expected_unreadable_task_state}, got {}",
                    state.unreadable_task_state
                )));
            }

            let expected_should_block =
                expected_should_block(state.mutating_tool, state.task_list_confirmed);
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "task_decomposition should_block must match decomposition policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }

            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "task_decomposition blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }

            if state.decision != TaskDecompositionDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "task_decomposition terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }

            Ok(())
        })
}

async fn classify_node(state: TaskDecompositionState) -> Result<TaskDecompositionState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_task_decomposition_graph() -> Result<TaskDecompositionGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("task_decomposition").await?;
    build_task_decomposition_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_task_decomposition_graph_with_ephemeral_sqlite(
) -> Result<TaskDecompositionGraph, String> {
    build_task_decomposition_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_task_decomposition_graph_with_database_path(
    db_path: &str,
) -> Result<TaskDecompositionGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_task_decomposition_graph_with_checkpointer(checkpointer).await
}

async fn build_task_decomposition_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<TaskDecompositionGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = task_decomposition_state_schema();
    let builder = StateGraphBuilder::<TaskDecompositionState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: TaskDecompositionState| async move {
                emit_decision_node_event("task_decomposition", CLASSIFY, &s.identifier)?;
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
            |s: TaskDecompositionState| async move {
                emit_decision_node_event("task_decomposition", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = TaskDecompositionDecision::Allow;
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
            |s: TaskDecompositionState| async move {
                emit_decision_node_event("task_decomposition", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = TaskDecompositionDecision::Block;
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
            |s: &TaskDecompositionState| match expected_decision(s) {
                TaskDecompositionDecision::Allow => ALLOW.into(),
                TaskDecompositionDecision::Block => BLOCK.into(),
                TaskDecompositionDecision::Unclassified => ALLOW.into(),
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

pub async fn run_task_decomposition_decision_report(
    compiled: &TaskDecompositionGraph,
    state: TaskDecompositionState,
) -> Result<TaskDecompositionGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "task_decomposition",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "task_decomposition",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "task_decomposition",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(TaskDecompositionGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: task_decomposition_graph_topology(compiled)?,
    })
}

pub fn task_decomposition_graph_topology(
    compiled: &TaskDecompositionGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("task_decomposition", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::task_decomposition_gate::{
        TaskDecompositionDecision as AppDecision, TaskDecompositionEvaluation,
    };

    fn edit_evaluation(task_list_confirmed: bool) -> TaskDecompositionEvaluation {
        TaskDecompositionEvaluation {
            tool: Some("Edit".to_string()),
            session_id: Some("session-1".to_string()),
            bash_command: None,
            allowed_tool: false,
            bash_tool: false,
            bash_command_present: false,
            mutating_tool: true,
            task_state_readable: true,
            task_list_confirmed,
            unreadable_task_state: false,
            should_block: !task_list_confirmed,
            decision: if task_list_confirmed {
                AppDecision::Allow
            } else {
                AppDecision::Block
            },
        }
    }

    fn bash_unreadable_evaluation() -> TaskDecompositionEvaluation {
        TaskDecompositionEvaluation {
            tool: Some("Bash".to_string()),
            session_id: None,
            bash_command: Some("git commit -m wip".to_string()),
            allowed_tool: false,
            bash_tool: true,
            bash_command_present: true,
            mutating_tool: true,
            task_state_readable: false,
            task_list_confirmed: false,
            unreadable_task_state: true,
            should_block: true,
            decision: AppDecision::Block,
        }
    }

    fn missing_session_edit_evaluation() -> TaskDecompositionEvaluation {
        TaskDecompositionEvaluation {
            tool: Some("Edit".to_string()),
            session_id: None,
            bash_command: None,
            allowed_tool: false,
            bash_tool: false,
            bash_command_present: false,
            mutating_tool: true,
            task_state_readable: false,
            task_list_confirmed: false,
            unreadable_task_state: true,
            should_block: true,
            decision: AppDecision::Block,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_live_task_list_allow() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = TaskDecompositionState::from_evaluation(
            "task-decomposition-allow",
            &edit_evaluation(true),
        );
        assert_eq!(state.tool.as_deref(), Some("Edit"));
        assert!(optional_hex_digest_present(&state.session_id_sha256));
        assert_eq!(state.bash_command_sha256, None);
        let run = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TaskDecompositionDecision::Allow);
        assert!(run
            .task_decomposition_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_task_list_block() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = TaskDecompositionState::from_evaluation(
            "task-decomposition-block",
            &edit_evaluation(false),
        );
        let run = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TaskDecompositionDecision::Block);
    }

    #[tokio::test]
    async fn graph_authorizes_missing_session_block_with_absent_session_hash() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = TaskDecompositionState::from_evaluation(
            "task-decomposition-missing-session",
            &missing_session_edit_evaluation(),
        );
        assert_eq!(state.session_id_sha256, None);
        let run = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TaskDecompositionDecision::Block);
        assert!(run.state.unreadable_task_state);
    }

    #[tokio::test]
    async fn graph_authorizes_unreadable_task_state_block() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = TaskDecompositionState::from_evaluation(
            "task-decomposition-unreadable",
            &bash_unreadable_evaluation(),
        );
        assert_eq!(state.tool.as_deref(), Some("Bash"));
        assert_eq!(state.session_id_sha256, None);
        assert!(optional_hex_digest_present(&state.bash_command_sha256));
        let run = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TaskDecompositionDecision::Block);
        assert!(run.state.unreadable_task_state);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_without_tasks() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TaskDecompositionState::from_evaluation(
            "task-decomposition-forged",
            &edit_evaluation(false),
        );
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_contradictory_task_state() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TaskDecompositionState::from_evaluation(
            "task-decomposition-contradictory",
            &bash_unreadable_evaluation(),
        );
        state.task_list_confirmed = true;
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("confirmed task list"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_tool_identity() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = edit_evaluation(true);
        eval.tool = None;
        let state =
            TaskDecompositionState::from_evaluation("task-decomposition-missing-tool", &eval);
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("tool identity"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_session_without_session_digest() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TaskDecompositionState::from_evaluation(
            "task-decomposition-missing-session-digest",
            &edit_evaluation(true),
        );
        state.session_id_sha256 = None;
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("session_id_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_session_with_extra_session_digest() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TaskDecompositionState::from_evaluation(
            "task-decomposition-extra-session-digest",
            &missing_session_edit_evaluation(),
        );
        state.session_id_sha256 = Some(sha256("session-1"));
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing session id"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_bash_command_without_command_digest() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TaskDecompositionState::from_evaluation(
            "task-decomposition-missing-command-digest",
            &bash_unreadable_evaluation(),
        );
        state.bash_command_sha256 = None;
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("bash_command_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_bash_command_with_extra_command_digest() {
        let graph = build_task_decomposition_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TaskDecompositionState::from_evaluation(
            "task-decomposition-extra-command-digest",
            &edit_evaluation(false),
        );
        state.bash_command_sha256 = Some(sha256("git commit -m wip"));
        let err = run_task_decomposition_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing Bash command"), "{err}");
    }
}
