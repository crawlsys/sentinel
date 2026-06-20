//! Graph-backed tasks.md auto-block authorization.
//!
//! The application hook computes deterministic facts for direct `Edit`/`Write`
//! calls against a project's root `tasks.md`: whether the file is in scope,
//! whether the existing auto block is present, and whether the requested edit
//! would mutate Sentinel-owned task content. This graph authorizes the
//! resulting allow/block decision through durable LangGraph checkpoints before
//! the CLI permits the file mutation.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::tasks_md_guard::TasksMdGuardEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TasksMdGuardDecision {
    #[default]
    Unclassified,
    Allow,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TasksMdGuardState {
    pub identifier: String,
    pub tool: Option<String>,
    pub guarded_tool: bool,
    pub edit_tool: bool,
    pub write_tool: bool,
    pub file_path_present: bool,
    pub file_path_sha256: Option<String>,
    pub project_tasks_md: bool,
    pub existing_file_present: bool,
    pub old_string_present: bool,
    pub content_present: bool,
    pub edit_overlaps_auto_block: bool,
    pub write_changes_auto_block: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: TasksMdGuardDecision,
}

impl TasksMdGuardState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &TasksMdGuardEvaluation,
    ) -> Self {
        let tool = evaluation
            .tool
            .as_deref()
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(ToString::to_string);
        let file_path_sha256 = evaluation
            .file_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty() && evaluation.file_path_present)
            .map(sha256);
        let blocking_finding_count = u64::from(expected_should_block(
            evaluation.guarded_tool,
            evaluation.file_path_present,
            evaluation.project_tasks_md,
            evaluation.edit_overlaps_auto_block,
            evaluation.write_changes_auto_block,
        ));
        Self {
            identifier: identifier.into(),
            tool,
            guarded_tool: evaluation.guarded_tool,
            edit_tool: evaluation.edit_tool,
            write_tool: evaluation.write_tool,
            file_path_present: evaluation.file_path_present,
            file_path_sha256,
            project_tasks_md: evaluation.project_tasks_md,
            existing_file_present: evaluation.existing_file_present,
            old_string_present: evaluation.old_string_present,
            content_present: evaluation.content_present,
            edit_overlaps_auto_block: evaluation.edit_overlaps_auto_block,
            write_changes_auto_block: evaluation.write_changes_auto_block,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: TasksMdGuardDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TasksMdGuardGraphRun {
    pub state: TasksMdGuardState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<TasksMdGuardState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct TasksMdGuardAuthorization {
    decision: TasksMdGuardDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl TasksMdGuardAuthorization {
    #[must_use]
    pub fn decision(&self) -> TasksMdGuardDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl TasksMdGuardGraphRun {
    #[must_use]
    pub fn tasks_md_guard_authorization(
        &self,
    ) -> Result<Option<TasksMdGuardAuthorization>, String> {
        if self.state.decision == TasksMdGuardDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "tasks_md_guard",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(TasksMdGuardAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK: &str = "block";

pub type TasksMdGuardGraph = CompilationResult<TasksMdGuardState>;

#[must_use]
pub fn tasks_md_guard_decision_label(decision: TasksMdGuardDecision) -> &'static str {
    match decision {
        TasksMdGuardDecision::Unclassified => "unclassified",
        TasksMdGuardDecision::Allow => "allow",
        TasksMdGuardDecision::Block => "block",
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
    guarded_tool: bool,
    file_path_present: bool,
    project_tasks_md: bool,
    edit_overlaps_auto_block: bool,
    write_changes_auto_block: bool,
) -> bool {
    guarded_tool
        && file_path_present
        && project_tasks_md
        && (edit_overlaps_auto_block || write_changes_auto_block)
}

fn expected_decision(state: &TasksMdGuardState) -> TasksMdGuardDecision {
    if state.should_block {
        TasksMdGuardDecision::Block
    } else {
        TasksMdGuardDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "tasks_md_guard")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn tasks_md_guard_state_schema() -> StateSchema<TasksMdGuardState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "guarded_tool",
                "edit_tool",
                "write_tool",
                "file_path_present",
                "file_path_sha256",
                "project_tasks_md",
                "existing_file_present",
                "old_string_present",
                "content_present",
                "edit_overlaps_auto_block",
                "write_changes_auto_block",
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
                "guarded_tool": { "type": "boolean" },
                "edit_tool": { "type": "boolean" },
                "write_tool": { "type": "boolean" },
                "file_path_present": { "type": "boolean" },
                "file_path_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "project_tasks_md": { "type": "boolean" },
                "existing_file_present": { "type": "boolean" },
                "old_string_present": { "type": "boolean" },
                "content_present": { "type": "boolean" },
                "edit_overlaps_auto_block": { "type": "boolean" },
                "write_changes_auto_block": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "tasks_md_guard",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &TasksMdGuardState| {
            let tool = state
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .ok_or_else(|| {
                    StateError::ValidationFailed(
                        "LangGraph tool-authority state requires concrete tool identity"
                            .to_string(),
                    )
                })?;
            if state.guarded_tool {
                if state.edit_tool == state.write_tool {
                    return Err(StateError::ValidationFailed(
                        "tasks_md_guard guarded tool must be exactly one of Edit/Write".to_string(),
                    ));
                }
                if tool != "Edit" && tool != "Write" {
                    return Err(StateError::ValidationFailed(format!(
                        "tasks_md_guard guarded tool requires Edit/Write, got {}",
                        tool
                    )));
                }
            } else if state.edit_tool
                || state.write_tool
                || state.project_tasks_md
                || state.existing_file_present
                || state.old_string_present
                || state.content_present
                || state.edit_overlaps_auto_block
                || state.write_changes_auto_block
                || state.blocking_finding_count > 0
                || state.should_block
            {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard non-guarded tool cannot carry guard findings".to_string(),
                ));
            }
            if state.file_path_present {
                if !optional_hex_digest_present(&state.file_path_sha256) {
                    return Err(StateError::ValidationFailed(
                        "tasks_md_guard file_path_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.file_path_sha256.is_some()
                || state.project_tasks_md
                || state.existing_file_present
                || state.old_string_present
                || state.content_present
                || state.edit_overlaps_auto_block
                || state.write_changes_auto_block
                || state.blocking_finding_count > 0
                || state.should_block
            {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard missing path cannot carry file findings".to_string(),
                ));
            }
            if state.project_tasks_md && (!state.guarded_tool || !state.file_path_present) {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard project tasks.md requires guarded tool and file path"
                        .to_string(),
                ));
            }
            if !state.project_tasks_md
                && (state.existing_file_present
                    || state.old_string_present
                    || state.content_present
                    || state.edit_overlaps_auto_block
                    || state.write_changes_auto_block
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard non-project tasks path cannot carry auto-block findings"
                        .to_string(),
                ));
            }
            if state.edit_overlaps_auto_block {
                if !state.edit_tool || !state.existing_file_present || !state.old_string_present {
                    return Err(StateError::ValidationFailed(
                        "tasks_md_guard edit overlap requires Edit, existing file, and old_string"
                            .to_string(),
                    ));
                }
                if state.write_changes_auto_block {
                    return Err(StateError::ValidationFailed(
                        "tasks_md_guard edit and write block findings are mutually exclusive"
                            .to_string(),
                    ));
                }
            }
            if state.write_changes_auto_block && !state.write_tool {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard write change requires Write tool".to_string(),
                ));
            }
            if state.edit_tool && state.content_present {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard Edit state cannot carry Write content".to_string(),
                ));
            }
            if state.write_tool && state.old_string_present {
                return Err(StateError::ValidationFailed(
                    "tasks_md_guard Write state cannot carry Edit old_string".to_string(),
                ));
            }
            let expected_should_block = expected_should_block(
                state.guarded_tool,
                state.file_path_present,
                state.project_tasks_md,
                state.edit_overlaps_auto_block,
                state.write_changes_auto_block,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "tasks_md_guard should_block must match tasks.md policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "tasks_md_guard blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != TasksMdGuardDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "tasks_md_guard terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: TasksMdGuardState) -> Result<TasksMdGuardState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_tasks_md_guard_graph() -> Result<TasksMdGuardGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("tasks_md_guard").await?;
    build_tasks_md_guard_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_tasks_md_guard_graph_with_ephemeral_sqlite() -> Result<TasksMdGuardGraph, String> {
    build_tasks_md_guard_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_tasks_md_guard_graph_with_database_path(
    db_path: &str,
) -> Result<TasksMdGuardGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_tasks_md_guard_graph_with_checkpointer(checkpointer).await
}

async fn build_tasks_md_guard_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<TasksMdGuardGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = tasks_md_guard_state_schema();
    let builder = StateGraphBuilder::<TasksMdGuardState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: TasksMdGuardState| async move {
                emit_decision_node_event("tasks_md_guard", CLASSIFY, &s.identifier)?;
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
            |s: TasksMdGuardState| async move {
                emit_decision_node_event("tasks_md_guard", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = TasksMdGuardDecision::Allow;
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
            |s: TasksMdGuardState| async move {
                emit_decision_node_event("tasks_md_guard", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = TasksMdGuardDecision::Block;
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
        .add_conditional_edge(CLASSIFY, |s: &TasksMdGuardState| {
            match expected_decision(s) {
                TasksMdGuardDecision::Allow => ALLOW.into(),
                TasksMdGuardDecision::Block => BLOCK.into(),
                TasksMdGuardDecision::Unclassified => ALLOW.into(),
            }
        })
        .add_edge(ALLOW, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_tasks_md_guard_decision_report(
    compiled: &TasksMdGuardGraph,
    state: TasksMdGuardState,
) -> Result<TasksMdGuardGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "tasks_md_guard",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "tasks_md_guard", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "tasks_md_guard",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(TasksMdGuardGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: tasks_md_guard_graph_topology(compiled)?,
    })
}

pub fn tasks_md_guard_graph_topology(
    compiled: &TasksMdGuardGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("tasks_md_guard", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::tasks_md_guard::TasksMdGuardDecision as AppDecision;

    fn edit_evaluation(block: bool) -> TasksMdGuardEvaluation {
        TasksMdGuardEvaluation {
            tool: Some("Edit".to_string()),
            file_path: Some("/repo/tasks.md".to_string()),
            guarded_tool: true,
            edit_tool: true,
            write_tool: false,
            file_path_present: true,
            project_tasks_md: true,
            existing_file_present: true,
            old_string_present: true,
            content_present: false,
            edit_overlaps_auto_block: block,
            write_changes_auto_block: false,
            should_block: block,
            decision: if block {
                AppDecision::Block
            } else {
                AppDecision::Allow
            },
        }
    }

    fn write_evaluation(block: bool) -> TasksMdGuardEvaluation {
        TasksMdGuardEvaluation {
            tool: Some("Write".to_string()),
            file_path: Some("/repo/tasks.md".to_string()),
            guarded_tool: true,
            edit_tool: false,
            write_tool: true,
            file_path_present: true,
            project_tasks_md: true,
            existing_file_present: true,
            old_string_present: false,
            content_present: true,
            edit_overlaps_auto_block: false,
            write_changes_auto_block: block,
            should_block: block,
            decision: if block {
                AppDecision::Block
            } else {
                AppDecision::Allow
            },
        }
    }

    fn missing_path_evaluation() -> TasksMdGuardEvaluation {
        TasksMdGuardEvaluation {
            tool: Some("Bash".to_string()),
            file_path: None,
            guarded_tool: false,
            edit_tool: false,
            write_tool: false,
            file_path_present: false,
            project_tasks_md: false,
            existing_file_present: false,
            old_string_present: false,
            content_present: false,
            edit_overlaps_auto_block: false,
            write_changes_auto_block: false,
            should_block: false,
            decision: AppDecision::Allow,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_edit_block() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            TasksMdGuardState::from_evaluation("tasks-md-edit-block", &edit_evaluation(true));
        let run = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TasksMdGuardDecision::Block);
        assert!(run
            .tasks_md_guard_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_write_allow() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            TasksMdGuardState::from_evaluation("tasks-md-write-allow", &write_evaluation(false));
        let run = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TasksMdGuardDecision::Allow);
    }

    #[tokio::test]
    async fn graph_authorizes_missing_file_path_without_digest() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            TasksMdGuardState::from_evaluation("tasks-md-missing-path", &missing_path_evaluation());
        let run = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TasksMdGuardDecision::Allow);
        assert!(run.state.file_path_sha256.is_none());
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_for_blocking_edit() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            TasksMdGuardState::from_evaluation("tasks-md-forged-edit", &edit_evaluation(true));
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_write_and_edit_findings_together() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            TasksMdGuardState::from_evaluation("tasks-md-conflicting", &edit_evaluation(true));
        state.write_changes_auto_block = true;
        let err = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_tool_identity() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            TasksMdGuardState::from_evaluation("tasks-md-missing-tool", &edit_evaluation(false));
        state.tool = None;
        let err = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("concrete tool identity"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_file_path_without_digest() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TasksMdGuardState::from_evaluation(
            "tasks-md-present-path-missing-digest",
            &edit_evaluation(false),
        );
        state.file_path_sha256 = None;
        let err = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("file_path_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_absent_file_path_with_extra_digest() {
        let graph = build_tasks_md_guard_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = TasksMdGuardState::from_evaluation(
            "tasks-md-absent-path-extra-digest",
            &missing_path_evaluation(),
        );
        state.file_path_sha256 = Some(sha256("/repo/tasks.md"));
        let err = run_tasks_md_guard_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing path"), "{err}");
    }
}
