//! Graph-backed authorization for operational CLI/MCP maintenance tools.
//!
//! These tools mutate local Sentinel/Claude operational state rather than a
//! skill workflow. The graph checkpoints the operation/result pair so a
//! successful maintenance command has durable LangGraph evidence too.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OperationalToolDecision {
    #[default]
    Unclassified,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalToolState {
    pub identifier: String,
    pub operation: String,
    pub input_sha256: String,
    pub input_len: u64,
    pub result_sha256: String,
    pub result_len: u64,
    pub result_present: bool,
    pub decision: OperationalToolDecision,
}

impl OperationalToolState {
    #[must_use]
    pub fn from_result(
        identifier: impl Into<String>,
        operation: impl Into<String>,
        input: &Value,
        result: &Value,
    ) -> Self {
        let input_bytes = json_bytes(input);
        let result_bytes = json_bytes(result);
        Self {
            identifier: identifier.into(),
            operation: operation.into(),
            input_sha256: sha256_bytes(&input_bytes),
            input_len: input_bytes.len() as u64,
            result_sha256: sha256_bytes(&result_bytes),
            result_len: result_bytes.len() as u64,
            result_present: value_present(result),
            decision: OperationalToolDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationalToolGraphRun {
    pub state: OperationalToolState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<OperationalToolState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct OperationalToolAuthorization {
    decision: OperationalToolDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl OperationalToolAuthorization {
    #[must_use]
    pub fn decision(&self) -> OperationalToolDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl OperationalToolGraphRun {
    #[must_use]
    pub fn operational_tool_authorization(
        &self,
    ) -> Result<Option<OperationalToolAuthorization>, String> {
        if self.state.decision == OperationalToolDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "operational_tool",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(OperationalToolAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const COMPLETED: &str = "completed";

pub type OperationalToolGraph = CompilationResult<OperationalToolState>;

#[must_use]
pub fn operational_tool_decision_label(decision: OperationalToolDecision) -> &'static str {
    match decision {
        OperationalToolDecision::Unclassified => "unclassified",
        OperationalToolDecision::Completed => "completed",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    sha256_bytes(&json_bytes(value))
}

fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("operational tool graph JSON value must serialize")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn value_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(_) | Value::Number(_) => true,
        Value::String(value) => !value.trim().is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "operational_tool")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn operational_tool_state_schema() -> StateSchema<OperationalToolState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "operation",
                "input_sha256",
                "input_len",
                "result_sha256",
                "result_len",
                "result_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "operation": {
                    "type": "string",
                    "enum": [
                        "regenerate_claude_md",
                        "edit_claude_md_template",
                        "restart_all_mcps",
                        "store_install_skill",
                        "store_uninstall_skill"
                    ]
                },
                "input_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "input_len": { "type": "integer", "minimum": 1 },
                "result_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "result_len": { "type": "integer", "minimum": 1 },
                "result_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Completed"]
                }
            },
            "x-sentinel": {
                "graph": "operational_tool",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &OperationalToolState| {
            if !matches!(
                state.operation.as_str(),
                "regenerate_claude_md"
                    | "edit_claude_md_template"
                    | "restart_all_mcps"
                    | "store_install_skill"
                    | "store_uninstall_skill"
            ) {
                return Err(StateError::ValidationFailed(
                    "operational tool operation is not registered".to_string(),
                ));
            }
            if state.input_len == 0 {
                return Err(StateError::ValidationFailed(
                    "operational tool input digest requires serialized input".to_string(),
                ));
            }
            if state.result_len == 0 || !state.result_present {
                return Err(StateError::ValidationFailed(
                    "operational tool completed result requires non-empty output".to_string(),
                ));
            }
            for (name, digest) in [
                ("input_sha256", &state.input_sha256),
                ("result_sha256", &state.result_sha256),
            ] {
                if !hex_digest_present(digest) {
                    return Err(StateError::ValidationFailed(format!(
                        "operational tool {name} must be a 64-character hex digest"
                    )));
                }
            }
            Ok(())
        })
}

async fn classify_node(state: OperationalToolState) -> Result<OperationalToolState, NodeError> {
    let mut next = state;
    next.decision = OperationalToolDecision::Completed;
    Ok(next)
}

async fn terminal_node(state: OperationalToolState) -> Result<OperationalToolState, NodeError> {
    let mut next = state;
    next.decision = OperationalToolDecision::Completed;
    Ok(next)
}

pub async fn build_operational_tool_graph() -> Result<OperationalToolGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("operational_tool").await?;
    build_operational_tool_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_operational_tool_graph_with_ephemeral_sqlite() -> Result<OperationalToolGraph, String>
{
    build_operational_tool_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_operational_tool_graph_with_database_path(
    db_path: &str,
) -> Result<OperationalToolGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_operational_tool_graph_with_checkpointer(checkpointer).await
}

async fn build_operational_tool_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<OperationalToolGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = operational_tool_state_schema();
    let builder = StateGraphBuilder::<OperationalToolState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: OperationalToolState| async move {
                emit_decision_node_event("operational_tool", CLASSIFY, &s.identifier)?;
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
            COMPLETED,
            |s: OperationalToolState| async move {
                emit_decision_node_event("operational_tool", COMPLETED, &s.identifier)?;
                terminal_node(s).await
            },
            node_config(
                COMPLETED,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |_s: &OperationalToolState| COMPLETED.into())
        .add_edge(COMPLETED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_operational_tool_decision_report(
    compiled: &OperationalToolGraph,
    state: OperationalToolState,
) -> Result<OperationalToolGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "operational_tool",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "operational_tool", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "operational_tool",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(OperationalToolGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: operational_tool_graph_topology(compiled)?,
    })
}

pub fn operational_tool_graph_topology(
    compiled: &OperationalToolGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("operational_tool", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn graph_authorizes_operational_tool_result() {
        let graph = build_operational_tool_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        for operation in [
            "restart_all_mcps",
            "store_install_skill",
            "store_uninstall_skill",
        ] {
            let state = OperationalToolState::from_result(
                "ops-test",
                operation,
                &serde_json::json!({}),
                &serde_json::json!({
                    "touched": [],
                    "skipped": [],
                    "touched_count": 0,
                    "skipped_count": 0
                }),
            );
            let run = run_operational_tool_decision_report(&graph, state)
                .await
                .unwrap();
            assert_eq!(run.state.decision, OperationalToolDecision::Completed);
            assert!(run
                .operational_tool_authorization()
                .unwrap()
                .unwrap()
                .checkpoint_ref()
                .contains('#'));
        }
    }

    #[tokio::test]
    async fn graph_schema_rejects_unknown_operation() {
        let graph = build_operational_tool_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = OperationalToolState::from_result(
            "ops-forged",
            "unknown",
            &serde_json::json!({}),
            &serde_json::json!({"ok": true}),
        );
        let err = run_operational_tool_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("operation"), "{err}");
    }
}
