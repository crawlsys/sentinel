//! Graph-backed audit for local operational API read responses.
//!
//! These endpoints expose local Sentinel operational state that is not a skill
//! workflow: marketplace scans, hook counters, hook health, and local metrics
//! logs. The graph checkpoints each rendered response so the API boundary
//! carries durable LangGraph evidence instead of returning raw filesystem or
//! in-memory projections.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationalApiReadSurface {
    RootHealth,
    Scan,
    Validation,
    Counts,
    Rescan,
    HookStats,
    HookHealth,
    Logs,
    Config,
    MemoryStatus,
    MemoryPrecomputed,
    MemoryInjected,
    MemoryDaemonStats,
    MemoryDaemonHealth,
    StoreBrowse,
    StorePreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OperationalApiReadDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationalApiReadState {
    pub identifier: String,
    pub surface: OperationalApiReadSurface,
    pub response_sha256: String,
    pub response_len: u64,
    pub response_object: bool,
    pub root_health_present: bool,
    pub scan_snapshot_present: bool,
    pub validation_report_present: bool,
    pub counts_present: bool,
    pub hook_stats_present: bool,
    pub hook_health_present: bool,
    pub logs_present: bool,
    pub config_present: bool,
    pub memory_status_present: bool,
    pub memory_state_file_present: bool,
    pub memory_proxy_present: bool,
    pub store_browse_present: bool,
    pub store_preview_present: bool,
    pub error_present: bool,
    pub entries_count: Option<u64>,
    pub total_count: Option<u64>,
    pub categories_count: Option<u64>,
    pub decision: OperationalApiReadDecision,
}

impl OperationalApiReadState {
    #[must_use]
    pub fn from_response(
        surface: OperationalApiReadSurface,
        identifier: impl Into<String>,
        response: &Value,
    ) -> Self {
        let response_bytes = json_bytes(response);
        let nested_counts_present = response.get("counts").is_some_and(component_counts_present);
        let direct_counts_present = component_counts_present(response);
        let nested_validation_present = response
            .get("validation")
            .is_some_and(validation_report_present);
        let direct_validation_present = validation_report_present(response);
        let entries_count = response
            .get("entries")
            .and_then(Value::as_array)
            .map(|entries| entries.len() as u64);

        Self {
            identifier: identifier.into(),
            surface,
            response_sha256: sha256_bytes(&response_bytes),
            response_len: response_bytes.len() as u64,
            response_object: response.as_object().is_some(),
            root_health_present: root_health_present(response),
            scan_snapshot_present: nonempty_string_field(response, "version")
                && nested_counts_present
                && nested_validation_present,
            validation_report_present: direct_validation_present,
            counts_present: direct_counts_present,
            hook_stats_present: hook_stats_present(response),
            hook_health_present: hook_health_present(response),
            logs_present: logs_response_present(response),
            config_present: config_response_present(response),
            memory_status_present: memory_status_present(response),
            memory_state_file_present: memory_state_file_present(response),
            memory_proxy_present: memory_proxy_present(response),
            store_browse_present: store_browse_present(response),
            store_preview_present: store_preview_present(response),
            error_present: nonempty_string_field(response, "error"),
            entries_count,
            total_count: response.get("total").and_then(Value::as_u64),
            categories_count: response
                .get("categories")
                .and_then(Value::as_object)
                .map(|categories| categories.len() as u64),
            decision: OperationalApiReadDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationalApiReadGraphRun {
    pub state: OperationalApiReadState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<OperationalApiReadState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct OperationalApiReadAuthorization {
    decision: OperationalApiReadDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl OperationalApiReadAuthorization {
    #[must_use]
    pub fn decision(&self) -> OperationalApiReadDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl OperationalApiReadGraphRun {
    pub fn operational_api_read_authorization(
        &self,
    ) -> Result<Option<OperationalApiReadAuthorization>, String> {
        if self.state.decision == OperationalApiReadDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "operational_api_read",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(OperationalApiReadAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type OperationalApiReadGraph = CompilationResult<OperationalApiReadState>;

#[must_use]
pub fn operational_api_read_decision_label(decision: OperationalApiReadDecision) -> &'static str {
    match decision {
        OperationalApiReadDecision::Unclassified => "unclassified",
        OperationalApiReadDecision::Verified => "verified",
    }
}

#[must_use]
pub fn operational_api_read_surface_label(surface: OperationalApiReadSurface) -> &'static str {
    match surface {
        OperationalApiReadSurface::RootHealth => "root_health",
        OperationalApiReadSurface::Scan => "scan",
        OperationalApiReadSurface::Validation => "validation",
        OperationalApiReadSurface::Counts => "counts",
        OperationalApiReadSurface::Rescan => "rescan",
        OperationalApiReadSurface::HookStats => "hook_stats",
        OperationalApiReadSurface::HookHealth => "hook_health",
        OperationalApiReadSurface::Logs => "logs",
        OperationalApiReadSurface::Config => "config",
        OperationalApiReadSurface::MemoryStatus => "memory_status",
        OperationalApiReadSurface::MemoryPrecomputed => "memory_precomputed",
        OperationalApiReadSurface::MemoryInjected => "memory_injected",
        OperationalApiReadSurface::MemoryDaemonStats => "memory_daemon_stats",
        OperationalApiReadSurface::MemoryDaemonHealth => "memory_daemon_health",
        OperationalApiReadSurface::StoreBrowse => "store_browse",
        OperationalApiReadSurface::StorePreview => "store_preview",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    sha256_bytes(&json_bytes(value))
}

fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("operational API read graph JSON value must serialize")
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn nonempty_string_field(value: &Value, field: &str) -> bool {
    value
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn number_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_u64).is_some()
}

fn object_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_object).is_some()
}

fn array_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_array).is_some()
}

fn component_counts_present(value: &Value) -> bool {
    [
        "skills",
        "hooks",
        "commands",
        "agents",
        "mcpServers",
        "mcpRepos",
        "cliRepos",
    ]
    .iter()
    .all(|field| number_field(value, field))
}

fn validation_report_present(value: &Value) -> bool {
    nonempty_string_field(value, "timestamp")
        && number_field(value, "duration_ms")
        && number_field(value, "passed")
        && number_field(value, "failed")
        && number_field(value, "warned")
        && array_field(value, "results")
}

fn hook_stats_present(value: &Value) -> bool {
    number_field(value, "total_invocations")
        && number_field(value, "total_blocked")
        && object_field(value, "per_hook")
        && object_field(value, "per_hook_time_ms")
}

fn hook_health_present(value: &Value) -> bool {
    value.get("active").and_then(Value::as_bool).is_some()
        && nonempty_string_field(value, "session_id")
        && nonempty_string_field(value, "uptime_since")
}

fn logs_response_present(value: &Value) -> bool {
    number_field(value, "total")
        && number_field(value, "offset")
        && number_field(value, "limit")
        && object_field(value, "categories")
        && array_field(value, "entries")
}

fn root_health_present(value: &Value) -> bool {
    value.get("status").and_then(Value::as_str) == Some("ok")
        && nonempty_string_field(value, "version")
        && value.get("engine").and_then(Value::as_str) == Some("sentinel")
}

fn config_response_present(value: &Value) -> bool {
    array_field(value, "hooks")
        && array_field(value, "workflows")
        && value
            .get("hooksTomlExists")
            .and_then(Value::as_bool)
            .is_some()
        && value
            .get("workflowsTomlExists")
            .and_then(Value::as_bool)
            .is_some()
}

fn memory_status_present(value: &Value) -> bool {
    value
        .get("qdrant_configured")
        .and_then(Value::as_bool)
        .is_some()
        && object_field(value, "precomputed")
        && object_field(value, "last_injected")
}

fn memory_state_file_present(value: &Value) -> bool {
    nonempty_string_field(value, "state_file")
        && value.get("present").and_then(Value::as_bool).is_some()
        && value.get("value").is_some()
}

fn memory_proxy_present(value: &Value) -> bool {
    let has_daemon_url = nonempty_string_field(value, "daemon_url");
    let has_success_shape = number_field(value, "upstream_status") && value.get("body").is_some();
    let has_error_shape = nonempty_string_field(value, "error")
        && value
            .get("reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.trim().is_empty());
    has_daemon_url && (has_success_shape || has_error_shape)
}

fn store_browse_present(value: &Value) -> bool {
    nonempty_string_field(value, "owner")
        && nonempty_string_field(value, "repo")
        && array_field(value, "skills")
}

fn store_preview_present(value: &Value) -> bool {
    nonempty_string_field(value, "name")
        && nonempty_string_field(value, "dir_name")
        && value.get("content").and_then(Value::as_str).is_some()
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "operational_api_read")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn operational_api_read_state_schema() -> StateSchema<OperationalApiReadState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "surface",
                "response_sha256",
                "response_len",
                "response_object",
                "root_health_present",
                "scan_snapshot_present",
                "validation_report_present",
                "counts_present",
                "hook_stats_present",
                "hook_health_present",
                "logs_present",
                "config_present",
                "memory_status_present",
                "memory_state_file_present",
                "memory_proxy_present",
                "store_browse_present",
                "store_preview_present",
                "error_present",
                "entries_count",
                "total_count",
                "categories_count",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "surface": {
                    "type": "string",
                    "enum": [
                        "RootHealth",
                        "Scan",
                        "Validation",
                        "Counts",
                        "Rescan",
                        "HookStats",
                        "HookHealth",
                        "Logs",
                        "Config",
                        "MemoryStatus",
                        "MemoryPrecomputed",
                        "MemoryInjected",
                        "MemoryDaemonStats",
                        "MemoryDaemonHealth",
                        "StoreBrowse",
                        "StorePreview"
                    ]
                },
                "response_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "response_len": { "type": "integer", "minimum": 1 },
                "response_object": { "type": "boolean" },
                "root_health_present": { "type": "boolean" },
                "scan_snapshot_present": { "type": "boolean" },
                "validation_report_present": { "type": "boolean" },
                "counts_present": { "type": "boolean" },
                "hook_stats_present": { "type": "boolean" },
                "hook_health_present": { "type": "boolean" },
                "logs_present": { "type": "boolean" },
                "config_present": { "type": "boolean" },
                "memory_status_present": { "type": "boolean" },
                "memory_state_file_present": { "type": "boolean" },
                "memory_proxy_present": { "type": "boolean" },
                "store_browse_present": { "type": "boolean" },
                "store_preview_present": { "type": "boolean" },
                "error_present": { "type": "boolean" },
                "entries_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "total_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "categories_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Verified"]
                }
            },
            "x-sentinel": {
                "graph": "operational_api_read",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &OperationalApiReadState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "operational API read requires a non-empty identifier".to_string(),
                ));
            }
            if state.response_len == 0 || !hex_digest_present(&state.response_sha256) {
                return Err(StateError::ValidationFailed(
                    "operational API read response digest must identify a serialized response"
                        .to_string(),
                ));
            }
            if !state.response_object {
                return Err(StateError::ValidationFailed(
                    "operational API read response must be a JSON object".to_string(),
                ));
            }
            match state.surface {
                OperationalApiReadSurface::RootHealth => {
                    if !state.root_health_present {
                        return Err(StateError::ValidationFailed(
                            "operational API root health response requires health fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::Scan | OperationalApiReadSurface::Rescan => {
                    if !state.scan_snapshot_present {
                        return Err(StateError::ValidationFailed(
                            "operational API scan response requires marketplace snapshot fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::Validation => {
                    if !state.validation_report_present {
                        return Err(StateError::ValidationFailed(
                            "operational API validation response requires validation report fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::Counts => {
                    if !state.counts_present {
                        return Err(StateError::ValidationFailed(
                            "operational API counts response requires component count fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::HookStats => {
                    if !state.hook_stats_present {
                        return Err(StateError::ValidationFailed(
                            "operational API hook stats response requires hook counter fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::HookHealth => {
                    if !state.hook_health_present {
                        return Err(StateError::ValidationFailed(
                            "operational API hook health response requires session health fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::Logs => {
                    if !state.logs_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "operational API logs response requires log page fields or a log read error"
                                .to_string(),
                        ));
                    }
                    if state.logs_present {
                        if let (Some(total), Some(entries)) =
                            (state.total_count, state.entries_count)
                        {
                            if entries > total {
                                return Err(StateError::ValidationFailed(
                                    "operational API logs response entries exceed total"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
                OperationalApiReadSurface::Config => {
                    if !state.config_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "operational API config response requires config summary fields or a config error"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::MemoryStatus => {
                    if !state.memory_status_present {
                        return Err(StateError::ValidationFailed(
                            "operational API memory status response requires memory summary fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::MemoryPrecomputed
                | OperationalApiReadSurface::MemoryInjected => {
                    if !state.memory_state_file_present {
                        return Err(StateError::ValidationFailed(
                            "operational API memory state response requires explicit state file envelope"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::MemoryDaemonStats
                | OperationalApiReadSurface::MemoryDaemonHealth => {
                    if !state.memory_proxy_present {
                        return Err(StateError::ValidationFailed(
                            "operational API memory daemon response requires proxy envelope fields"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::StoreBrowse => {
                    if !state.store_browse_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "operational API store browse response requires browse fields or error"
                                .to_string(),
                        ));
                    }
                }
                OperationalApiReadSurface::StorePreview => {
                    if !state.store_preview_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "operational API store preview response requires preview fields or error"
                                .to_string(),
                        ));
                    }
                }
            }
            Ok(())
        })
}

async fn classify_node(
    state: OperationalApiReadState,
) -> Result<OperationalApiReadState, NodeError> {
    let mut next = state;
    next.decision = OperationalApiReadDecision::Verified;
    Ok(next)
}

async fn terminal_node(
    state: OperationalApiReadState,
) -> Result<OperationalApiReadState, NodeError> {
    let mut next = state;
    next.decision = OperationalApiReadDecision::Verified;
    Ok(next)
}

pub async fn build_operational_api_read_graph() -> Result<OperationalApiReadGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("operational_api_read").await?;
    build_operational_api_read_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_operational_api_read_graph_with_ephemeral_sqlite(
) -> Result<OperationalApiReadGraph, String> {
    build_operational_api_read_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_operational_api_read_graph_with_database_path(
    db_path: &str,
) -> Result<OperationalApiReadGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_operational_api_read_graph_with_checkpointer(checkpointer).await
}

async fn build_operational_api_read_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<OperationalApiReadGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = operational_api_read_state_schema();
    let builder = StateGraphBuilder::<OperationalApiReadState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: OperationalApiReadState| async move {
                emit_decision_node_event("operational_api_read", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            VERIFIED,
            |s: OperationalApiReadState| async move {
                emit_decision_node_event("operational_api_read", VERIFIED, &s.identifier)?;
                terminal_node(s).await
            },
            node_config(VERIFIED, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |_s: &OperationalApiReadState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_operational_api_read_decision_report(
    compiled: &OperationalApiReadGraph,
    state: OperationalApiReadState,
) -> Result<OperationalApiReadGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id(
        "operational_api_read",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "operational_api_read",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "operational_api_read",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(OperationalApiReadGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: operational_api_read_graph_topology(compiled)?,
    })
}

pub fn operational_api_read_graph_topology(
    compiled: &OperationalApiReadGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("operational_api_read", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_response() -> Value {
        serde_json::json!({
            "version": "1.0.0",
            "description": "local marketplace snapshot",
            "skills": [],
            "hooks": [],
            "agents": [],
            "commands": [],
            "mcpServers": [],
            "dependencyEdges": [],
            "validation": validation_response(),
            "counts": counts_response()
        })
    }

    fn validation_response() -> Value {
        serde_json::json!({
            "timestamp": "2026-06-18T12:00:00Z",
            "duration_ms": 3,
            "passed": 1,
            "failed": 0,
            "warned": 0,
            "results": []
        })
    }

    fn counts_response() -> Value {
        serde_json::json!({
            "skills": 0,
            "hooks": 0,
            "commands": 0,
            "agents": 0,
            "mcpServers": 0,
            "mcpRepos": 0,
            "cliRepos": 0
        })
    }

    fn hook_stats_response() -> Value {
        serde_json::json!({
            "total_invocations": 2,
            "total_blocked": 1,
            "per_hook": {
                "phase_gate": 2
            },
            "per_hook_time_ms": {
                "phase_gate": 10
            }
        })
    }

    fn hook_health_response() -> Value {
        serde_json::json!({
            "active": true,
            "session_id": "session-1",
            "active_skill": "linear",
            "uptime_since": "2026-06-18T12:00:00Z"
        })
    }

    fn logs_response() -> Value {
        serde_json::json!({
            "total": 1,
            "categories": {
                "activity": 1
            },
            "offset": 0,
            "limit": 200,
            "entries": [
                {
                    "ts": "2026-06-18T12:00:00Z",
                    "_category": "activity",
                    "_source": "activity-log.jsonl"
                }
            ]
        })
    }

    fn logs_error_response() -> Value {
        serde_json::json!({
            "error": "activity-log.jsonl: malformed JSONL at line 2"
        })
    }

    fn root_health_response() -> Value {
        serde_json::json!({
            "status": "ok",
            "version": "1.0.0",
            "engine": "sentinel"
        })
    }

    fn config_response() -> Value {
        serde_json::json!({
            "hooks": [],
            "workflows": [],
            "hooksTomlExists": true,
            "workflowsTomlExists": true,
            "workflowCount": 2
        })
    }

    fn config_error_response() -> Value {
        serde_json::json!({
            "graph_authority": "langgraph",
            "error": "authoritative workflows.toml load failed: missing phases",
            "hooksTomlExists": true,
            "workflowsTomlExists": true
        })
    }

    fn memory_status_response() -> Value {
        serde_json::json!({
            "qdrant_configured": true,
            "precomputed": {
                "query": "memory daemon",
                "timestamp": "2026-06-18T12:00:00Z",
                "hit_count": 2,
                "fresh": true
            },
            "last_injected": {
                "timestamp": "2026-06-18T12:00:00Z",
                "hit_count": 1,
                "user_prompt": "ship it"
            }
        })
    }

    fn memory_status_state_file_response() -> Value {
        serde_json::json!({
            "qdrant_configured": true,
            "precomputed": {
                "state_file": "precomputed-memories.json",
                "present": true,
                "value": {
                    "query": "memory daemon",
                    "timestamp": "2026-06-18T12:00:00Z",
                    "results": []
                }
            },
            "last_injected": {
                "state_file": "last-injected-memories.json",
                "present": false,
                "value": null
            }
        })
    }

    fn memory_state_file_response() -> Value {
        serde_json::json!({
            "state_file": "precomputed-memories.json",
            "present": true,
            "value": {
                "timestamp": "2026-06-18T12:00:00Z",
                "results": []
            }
        })
    }

    fn memory_proxy_response() -> Value {
        serde_json::json!({
            "daemon_url": "http://127.0.0.1:3011",
            "upstream_status": 200,
            "body": {
                "ok": true
            }
        })
    }

    fn store_browse_response() -> Value {
        serde_json::json!({
            "owner": "owner",
            "repo": "repo",
            "skills": []
        })
    }

    fn store_preview_response() -> Value {
        serde_json::json!({
            "name": "Skill",
            "dir_name": "skill",
            "description": "",
            "content": "# Skill"
        })
    }

    #[tokio::test]
    async fn graph_authorizes_all_operational_api_surfaces() {
        let graph = build_operational_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let cases = [
            (
                OperationalApiReadSurface::RootHealth,
                root_health_response(),
            ),
            (OperationalApiReadSurface::Scan, scan_response()),
            (OperationalApiReadSurface::Rescan, scan_response()),
            (OperationalApiReadSurface::Validation, validation_response()),
            (OperationalApiReadSurface::Counts, counts_response()),
            (OperationalApiReadSurface::HookStats, hook_stats_response()),
            (
                OperationalApiReadSurface::HookHealth,
                hook_health_response(),
            ),
            (OperationalApiReadSurface::Logs, logs_response()),
            (OperationalApiReadSurface::Logs, logs_error_response()),
            (OperationalApiReadSurface::Config, config_response()),
            (OperationalApiReadSurface::Config, config_error_response()),
            (
                OperationalApiReadSurface::MemoryStatus,
                memory_status_response(),
            ),
            (
                OperationalApiReadSurface::MemoryStatus,
                memory_status_state_file_response(),
            ),
            (
                OperationalApiReadSurface::MemoryPrecomputed,
                memory_state_file_response(),
            ),
            (
                OperationalApiReadSurface::MemoryInjected,
                memory_state_file_response(),
            ),
            (
                OperationalApiReadSurface::MemoryDaemonStats,
                memory_proxy_response(),
            ),
            (
                OperationalApiReadSurface::MemoryDaemonHealth,
                memory_proxy_response(),
            ),
            (
                OperationalApiReadSurface::StoreBrowse,
                store_browse_response(),
            ),
            (
                OperationalApiReadSurface::StorePreview,
                store_preview_response(),
            ),
        ];

        for (surface, response) in cases {
            let state =
                OperationalApiReadState::from_response(surface, format!("{surface:?}"), &response);
            let run = run_operational_api_read_decision_report(&graph, state)
                .await
                .unwrap();
            assert_eq!(run.state.decision, OperationalApiReadDecision::Verified);
            assert!(run
                .operational_api_read_authorization()
                .unwrap()
                .unwrap()
                .checkpoint_ref()
                .contains('#'));
        }
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let graph = build_operational_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = root_health_response();
        let state = OperationalApiReadState::from_response(
            OperationalApiReadSurface::RootHealth,
            "root-health",
            &response,
        );
        let mut run = run_operational_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        run.write_history.clear();

        let err = run
            .operational_api_read_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_rejects_mismatched_surface_shape() {
        let graph = build_operational_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = OperationalApiReadState::from_response(
            OperationalApiReadSurface::Counts,
            "bad-counts",
            &validation_response(),
        );
        let err = run_operational_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("counts response"), "{err}");
    }
}
