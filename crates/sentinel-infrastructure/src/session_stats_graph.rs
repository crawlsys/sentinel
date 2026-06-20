//! Graph-backed audit for the session stats MCP read surface.
//!
//! `sentinel__get_session_stats` is read-only, but it summarizes Sentinel's
//! runtime authority state. This graph validates the rendered stats payload and
//! checkpoints the read so callers get durable LangGraph evidence instead of an
//! uncheckpointed projection.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
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
pub enum SessionStatsDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatsState {
    pub identifier: String,
    pub session_id: Option<String>,
    pub stats_sha256: String,
    pub stats_len: u64,
    pub workflow_authority_present: bool,
    pub workflow_authority_langgraph: bool,
    pub active_skill: Option<String>,
    pub total_invocations: u64,
    pub total_blocked: u64,
    pub hook_count: u64,
    pub langgraph_workflows: Vec<String>,
    pub langgraph_workflow_count: u64,
    pub proof_chains: Vec<String>,
    pub proof_chain_count: u64,
    pub decision: SessionStatsDecision,
}

impl SessionStatsState {
    #[must_use]
    pub fn from_response(identifier: impl Into<String>, response: &Value) -> Self {
        let stats_bytes =
            serde_json::to_vec(response).expect("session stats response must serialize");
        let langgraph_workflows = string_array(response, "langgraph_workflows");
        let proof_chains = string_array(response, "proof_chains");
        Self {
            identifier: identifier.into(),
            session_id: response
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
                .map(ToString::to_string),
            stats_sha256: hex::encode(Sha256::digest(&stats_bytes)),
            stats_len: stats_bytes.len() as u64,
            workflow_authority_present: response.get("workflow_authority").is_some(),
            workflow_authority_langgraph: response
                .get("workflow_authority")
                .and_then(Value::as_str)
                == Some("langgraph"),
            active_skill: response
                .get("active_skill")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            total_invocations: response
                .get("total_invocations")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            total_blocked: response
                .get("total_blocked")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            hook_count: response
                .get("per_hook")
                .and_then(Value::as_object)
                .map_or(0, |hooks| hooks.len() as u64),
            langgraph_workflow_count: response
                .get("langgraph_workflow_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            langgraph_workflows,
            proof_chain_count: proof_chains.len() as u64,
            proof_chains,
            decision: SessionStatsDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionStatsGraphRun {
    pub state: SessionStatsState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SessionStatsState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct SessionStatsAuthorization {
    decision: SessionStatsDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl SessionStatsAuthorization {
    #[must_use]
    pub fn decision(&self) -> SessionStatsDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl SessionStatsGraphRun {
    #[must_use]
    pub fn session_stats_authorization(&self) -> Result<Option<SessionStatsAuthorization>, String> {
        if self.state.decision == SessionStatsDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "session_stats",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SessionStatsAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type SessionStatsGraph = CompilationResult<SessionStatsState>;

#[must_use]
pub fn session_stats_decision_label(decision: SessionStatsDecision) -> &'static str {
    match decision {
        SessionStatsDecision::Unclassified => "unclassified",
        SessionStatsDecision::Verified => "verified",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("session stats JSON value must serialize"),
    ))
}

fn string_array(response: &Value, key: &str) -> Vec<String> {
    match response.get(key).and_then(Value::as_array) {
        Some(values) => values
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        None => Vec::new(),
    }
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn sorted_nonempty(values: &[String]) -> bool {
    values.iter().all(|value| !value.trim().is_empty())
        && values.windows(2).all(|pair| pair[0] <= pair[1])
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "session_stats")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn session_stats_state_schema() -> StateSchema<SessionStatsState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "session_id",
                "stats_sha256",
                "stats_len",
                "workflow_authority_present",
                "workflow_authority_langgraph",
                "active_skill",
                "total_invocations",
                "total_blocked",
                "hook_count",
                "langgraph_workflows",
                "langgraph_workflow_count",
                "proof_chains",
                "proof_chain_count",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "session_id": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
                        { "type": "null" }
                    ]
                },
                "stats_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "stats_len": { "type": "integer", "minimum": 1 },
                "workflow_authority_present": { "type": "boolean" },
                "workflow_authority_langgraph": { "type": "boolean" },
                "active_skill": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
                        { "type": "null" }
                    ]
                },
                "total_invocations": { "type": "integer", "minimum": 0 },
                "total_blocked": { "type": "integer", "minimum": 0 },
                "hook_count": { "type": "integer", "minimum": 0 },
                "langgraph_workflows": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 }
                },
                "langgraph_workflow_count": { "type": "integer", "minimum": 0 },
                "proof_chains": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 }
                },
                "proof_chain_count": { "type": "integer", "minimum": 0 },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Verified"]
                }
            },
            "x-sentinel": {
                "graph": "session_stats",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SessionStatsState| {
            if state
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
                .is_none()
            {
                return Err(StateError::ValidationFailed(
                    "session stats response requires session_id".to_string(),
                ));
            }
            if state.stats_len == 0 || !hex_digest_present(&state.stats_sha256) {
                return Err(StateError::ValidationFailed(
                    "session stats response digest must identify a serialized response".to_string(),
                ));
            }
            if state.workflow_authority_present {
                return Err(StateError::ValidationFailed(
                    "session stats response must not declare workflow authority before read graph audit"
                        .to_string(),
                ));
            }
            if state.total_blocked > state.total_invocations {
                return Err(StateError::ValidationFailed(
                    "session stats blocked count cannot exceed invocation count".to_string(),
                ));
            }
            if state.langgraph_workflow_count != state.langgraph_workflows.len() as u64 {
                return Err(StateError::ValidationFailed(
                    "session stats workflow count must match workflow list".to_string(),
                ));
            }
            if !sorted_nonempty(&state.langgraph_workflows) {
                return Err(StateError::ValidationFailed(
                    "session stats workflow list must be sorted and contain only non-empty values"
                        .to_string(),
                ));
            }
            if state.proof_chain_count != state.proof_chains.len() as u64 {
                return Err(StateError::ValidationFailed(
                    "session stats proof chain count must match proof chain list".to_string(),
                ));
            }
            if !sorted_nonempty(&state.proof_chains) {
                return Err(StateError::ValidationFailed(
                    "session stats proof chain list must be sorted and contain only non-empty values"
                        .to_string(),
                ));
            }
            if !matches!(
                state.decision,
                SessionStatsDecision::Unclassified | SessionStatsDecision::Verified
            ) {
                return Err(StateError::ValidationFailed(
                    "session stats decision is invalid".to_string(),
                ));
            }
            Ok(())
        })
}

async fn classify_node(state: SessionStatsState) -> Result<SessionStatsState, NodeError> {
    let mut next = state;
    next.decision = SessionStatsDecision::Verified;
    Ok(next)
}

async fn terminal_node(state: SessionStatsState) -> Result<SessionStatsState, NodeError> {
    let mut next = state;
    next.decision = SessionStatsDecision::Verified;
    Ok(next)
}

pub async fn build_session_stats_graph() -> Result<SessionStatsGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("session_stats").await?;
    build_session_stats_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_session_stats_graph_with_ephemeral_sqlite() -> Result<SessionStatsGraph, String> {
    build_session_stats_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_session_stats_graph_with_database_path(
    db_path: &str,
) -> Result<SessionStatsGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_session_stats_graph_with_checkpointer(checkpointer).await
}

async fn build_session_stats_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SessionStatsGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = session_stats_state_schema();
    let builder = StateGraphBuilder::<SessionStatsState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: SessionStatsState| async move {
                emit_decision_node_event("session_stats", CLASSIFY, &s.identifier)?;
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
            VERIFIED,
            |s: SessionStatsState| async move {
                emit_decision_node_event("session_stats", VERIFIED, &s.identifier)?;
                terminal_node(s).await
            },
            node_config(
                VERIFIED,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |_s: &SessionStatsState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_session_stats_decision_report(
    compiled: &SessionStatsGraph,
    state: SessionStatsState,
) -> Result<SessionStatsGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "session_stats",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "session_stats", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "session_stats",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SessionStatsGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: session_stats_graph_topology(compiled)?,
    })
}

pub fn session_stats_graph_topology(
    compiled: &SessionStatsGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("session_stats", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Value {
        serde_json::json!({
            "session_id": "stats-session",
            "active_skill": "linear",
            "total_invocations": 4,
            "total_blocked": 1,
            "per_hook": {
                "PreToolUse": 3,
                "Stop": 1
            },
            "langgraph_workflows": ["linear"],
            "langgraph_workflow_count": 1,
            "proof_chains": ["linear"]
        })
    }

    #[tokio::test]
    async fn graph_authorizes_session_stats_response() {
        let graph = build_session_stats_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SessionStatsState::from_response("stats-session", &response());
        let run = run_session_stats_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SessionStatsDecision::Verified);
        assert!(run
            .session_stats_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_schema_rejects_explicit_workflow_authority_before_read_audit() {
        let graph = build_session_stats_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = response();
        forged["workflow_authority"] = serde_json::json!("langgraph");
        let state = SessionStatsState::from_response("stats-forged", &forged);
        let err = run_session_stats_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("before read graph audit"), "{err}");
    }
}
