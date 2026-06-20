//! Graph-backed audit for the WIP snapshot MCP read surface.
//!
//! `sentinel__get_wip_snapshot` is read-only, but its output can influence
//! operator planning. This graph validates the response shape and persists a
//! checkpoint so the read surface carries the same LangGraph authority evidence
//! as Sentinel's mutation and analytics tools.

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
pub enum WipSnapshotDecision {
    #[default]
    Unclassified,
    SnapshotPresent,
    NoSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WipSnapshotState {
    pub identifier: String,
    pub response_sha256: String,
    pub response_len: u64,
    pub snapshot_present: bool,
    pub captured_at_present: bool,
    pub total_wip: u64,
    pub team_count: u64,
    pub bottleneck_count: u64,
    pub message_present: bool,
    pub decision: WipSnapshotDecision,
}

impl WipSnapshotState {
    #[must_use]
    pub fn from_response(identifier: impl Into<String>, response: &Value) -> Self {
        let response_bytes =
            serde_json::to_vec(response).expect("WIP snapshot response must serialize");
        let snapshot_present = response
            .get("captured_at")
            .is_some_and(serde_json::Value::is_string);
        let captured_at_present = response
            .get("captured_at")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        let team_count = response
            .get("teams")
            .and_then(serde_json::Value::as_object)
            .map_or(0, |teams| teams.len() as u64);
        let bottleneck_count = response
            .get("bottlenecks")
            .and_then(serde_json::Value::as_array)
            .map_or(0, |bottlenecks| bottlenecks.len() as u64);
        let message_present = response
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        Self {
            identifier: identifier.into(),
            response_sha256: hex::encode(Sha256::digest(&response_bytes)),
            response_len: response_bytes.len() as u64,
            snapshot_present,
            captured_at_present,
            total_wip: response
                .get("total_wip")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            team_count,
            bottleneck_count,
            message_present,
            decision: WipSnapshotDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WipSnapshotGraphRun {
    pub state: WipSnapshotState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<WipSnapshotState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct WipSnapshotAuthorization {
    decision: WipSnapshotDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl WipSnapshotAuthorization {
    #[must_use]
    pub fn decision(&self) -> WipSnapshotDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl WipSnapshotGraphRun {
    #[must_use]
    pub fn wip_snapshot_authorization(&self) -> Result<Option<WipSnapshotAuthorization>, String> {
        if self.state.decision == WipSnapshotDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "wip_snapshot",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(WipSnapshotAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const SNAPSHOT_PRESENT: &str = "snapshot_present";
const NO_SNAPSHOT: &str = "no_snapshot";

pub type WipSnapshotGraph = CompilationResult<WipSnapshotState>;

#[must_use]
pub fn wip_snapshot_decision_label(decision: WipSnapshotDecision) -> &'static str {
    match decision {
        WipSnapshotDecision::Unclassified => "unclassified",
        WipSnapshotDecision::SnapshotPresent => "snapshot-present",
        WipSnapshotDecision::NoSnapshot => "no-snapshot",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("WIP snapshot JSON value must serialize"),
    ))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn expected_decision(state: &WipSnapshotState) -> WipSnapshotDecision {
    if state.snapshot_present {
        WipSnapshotDecision::SnapshotPresent
    } else {
        WipSnapshotDecision::NoSnapshot
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "wip_snapshot")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn wip_snapshot_state_schema() -> StateSchema<WipSnapshotState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "response_sha256",
                "response_len",
                "snapshot_present",
                "captured_at_present",
                "total_wip",
                "team_count",
                "bottleneck_count",
                "message_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "response_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "response_len": { "type": "integer", "minimum": 1 },
                "snapshot_present": { "type": "boolean" },
                "captured_at_present": { "type": "boolean" },
                "total_wip": { "type": "integer", "minimum": 0 },
                "team_count": { "type": "integer", "minimum": 0 },
                "bottleneck_count": { "type": "integer", "minimum": 0 },
                "message_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "SnapshotPresent", "NoSnapshot"]
                }
            },
            "x-sentinel": {
                "graph": "wip_snapshot",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &WipSnapshotState| {
            if state.response_len == 0 || !hex_digest_present(&state.response_sha256) {
                return Err(StateError::ValidationFailed(
                    "wip snapshot response digest must identify a serialized response".to_string(),
                ));
            }
            if state.snapshot_present && !state.captured_at_present {
                return Err(StateError::ValidationFailed(
                    "wip snapshot present response requires captured_at".to_string(),
                ));
            }
            if !state.snapshot_present && state.captured_at_present {
                return Err(StateError::ValidationFailed(
                    "wip snapshot absent response must not carry captured_at".to_string(),
                ));
            }
            if !state.snapshot_present
                && (state.total_wip != 0
                    || state.team_count != 0
                    || state.bottleneck_count != 0
                    || !state.message_present)
            {
                return Err(StateError::ValidationFailed(
                    "wip snapshot absent response requires only a no-data message".to_string(),
                ));
            }
            let expected = expected_decision(state);
            if state.decision != WipSnapshotDecision::Unclassified && state.decision != expected {
                return Err(StateError::ValidationFailed(format!(
                    "wip snapshot decision must be {}",
                    wip_snapshot_decision_label(expected)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: WipSnapshotState) -> Result<WipSnapshotState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

async fn terminal_node(
    state: WipSnapshotState,
    decision: WipSnapshotDecision,
) -> Result<WipSnapshotState, NodeError> {
    let mut next = state;
    next.decision = decision;
    Ok(next)
}

pub async fn build_wip_snapshot_graph() -> Result<WipSnapshotGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("wip_snapshot").await?;
    build_wip_snapshot_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_wip_snapshot_graph_with_ephemeral_sqlite() -> Result<WipSnapshotGraph, String> {
    build_wip_snapshot_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_wip_snapshot_graph_with_database_path(
    db_path: &str,
) -> Result<WipSnapshotGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_wip_snapshot_graph_with_checkpointer(checkpointer).await
}

async fn build_wip_snapshot_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<WipSnapshotGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = wip_snapshot_state_schema();
    let builder = StateGraphBuilder::<WipSnapshotState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: WipSnapshotState| async move {
                emit_decision_node_event("wip_snapshot", CLASSIFY, &s.identifier)?;
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
            SNAPSHOT_PRESENT,
            |s: WipSnapshotState| async move {
                emit_decision_node_event("wip_snapshot", SNAPSHOT_PRESENT, &s.identifier)?;
                terminal_node(s, WipSnapshotDecision::SnapshotPresent).await
            },
            node_config(
                SNAPSHOT_PRESENT,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            NO_SNAPSHOT,
            |s: WipSnapshotState| async move {
                emit_decision_node_event("wip_snapshot", NO_SNAPSHOT, &s.identifier)?;
                terminal_node(s, WipSnapshotDecision::NoSnapshot).await
            },
            node_config(
                NO_SNAPSHOT,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &WipSnapshotState| {
            match expected_decision(s) {
                WipSnapshotDecision::SnapshotPresent => SNAPSHOT_PRESENT.into(),
                WipSnapshotDecision::NoSnapshot | WipSnapshotDecision::Unclassified => {
                    NO_SNAPSHOT.into()
                }
            }
        })
        .add_edge(SNAPSHOT_PRESENT, END)
        .add_edge(NO_SNAPSHOT, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_wip_snapshot_decision_report(
    compiled: &WipSnapshotGraph,
    state: WipSnapshotState,
) -> Result<WipSnapshotGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "wip_snapshot",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "wip_snapshot", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "wip_snapshot",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(WipSnapshotGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: wip_snapshot_graph_topology(compiled)?,
    })
}

pub fn wip_snapshot_graph_topology(
    compiled: &WipSnapshotGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("wip_snapshot", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present_response() -> Value {
        serde_json::json!({
            "captured_at": "2026-06-18T12:00:00Z",
            "total_wip": 4,
            "teams": {
                "LEG": {
                    "In Progress": 3,
                    "Code Review": 1
                }
            },
            "bottlenecks": []
        })
    }

    fn missing_response() -> Value {
        serde_json::json!({
            "captured_at": null,
            "message": "no snapshot captured yet - poller has not run"
        })
    }

    #[tokio::test]
    async fn graph_authorizes_present_snapshot() {
        let graph = build_wip_snapshot_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = WipSnapshotState::from_response("wip-present", &present_response());
        let run = run_wip_snapshot_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, WipSnapshotDecision::SnapshotPresent);
        assert!(run
            .wip_snapshot_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_snapshot() {
        let graph = build_wip_snapshot_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = WipSnapshotState::from_response("wip-missing", &missing_response());
        let run = run_wip_snapshot_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, WipSnapshotDecision::NoSnapshot);
    }

    #[tokio::test]
    async fn graph_schema_rejects_absent_snapshot_without_message() {
        let graph = build_wip_snapshot_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = WipSnapshotState::from_response(
            "wip-forged",
            &serde_json::json!({
                "captured_at": null
            }),
        );
        let err = run_wip_snapshot_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("no-data message"), "{err}");
    }
}
