//! Graph-backed audit for local API session read responses.
//!
//! `/api/sentinel/sessions` and `/api/sentinel/sessions/{id}` expose
//! LangGraph-projected workflow authority. This graph checkpoints each rendered
//! read response so callers receive durable LangGraph evidence for that
//! authority claim instead of a plain JSON projection.

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
pub enum SessionApiReadSurface {
    Summary,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionApiReadDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionApiReadState {
    pub identifier: String,
    pub surface: SessionApiReadSurface,
    pub session_id: Option<String>,
    pub response_sha256: String,
    pub response_len: u64,
    pub workflow_authority_present: bool,
    pub workflow_authority_langgraph: bool,
    pub active: bool,
    pub active_skill_present: bool,
    pub file_present: bool,
    pub tool_calls: u64,
    pub phase_read_count: u64,
    pub langgraph_workflow_count: u64,
    pub langgraph_workflow_key_count: Option<u64>,
    pub proof_chain_count: u64,
    pub hook_stats_present: bool,
    pub decision: SessionApiReadDecision,
}

impl SessionApiReadState {
    #[must_use]
    pub fn from_response(
        surface: SessionApiReadSurface,
        identifier: impl Into<String>,
        response: &Value,
    ) -> Self {
        let response_bytes =
            serde_json::to_vec(response).expect("session API read response must serialize");
        let session_id = response
            .get("id")
            .or_else(|| response.get("session_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|session_id| !session_id.is_empty())
            .map(ToString::to_string);
        let langgraph_workflow_key_count = response
            .get("langgraph_workflows")
            .and_then(Value::as_object)
            .map(|workflows| workflows.len() as u64);

        Self {
            identifier: identifier.into(),
            surface,
            session_id,
            response_sha256: hex::encode(Sha256::digest(&response_bytes)),
            response_len: response_bytes.len() as u64,
            workflow_authority_present: response.get("workflow_authority").is_some(),
            workflow_authority_langgraph: response
                .get("workflow_authority")
                .and_then(Value::as_str)
                == Some("langgraph"),
            active: response
                .get("active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            active_skill_present: response
                .get("active_skill")
                .and_then(Value::as_str)
                .is_some_and(|skill| !skill.trim().is_empty()),
            file_present: response
                .get("file")
                .and_then(Value::as_str)
                .is_some_and(|file| !file.trim().is_empty()),
            tool_calls: response
                .get("tool_calls")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            phase_read_count: phase_read_count(response),
            langgraph_workflow_count: response
                .get("langgraph_workflow_count")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            langgraph_workflow_key_count,
            proof_chain_count: proof_chain_count(response),
            hook_stats_present: response.get("hook_stats").is_some(),
            decision: SessionApiReadDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionApiReadGraphRun {
    pub state: SessionApiReadState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SessionApiReadState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct SessionApiReadAuthorization {
    decision: SessionApiReadDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl SessionApiReadAuthorization {
    #[must_use]
    pub fn decision(&self) -> SessionApiReadDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl SessionApiReadGraphRun {
    #[must_use]
    pub fn session_api_read_authorization(
        &self,
    ) -> Result<Option<SessionApiReadAuthorization>, String> {
        if self.state.decision == SessionApiReadDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "session_api_read",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SessionApiReadAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type SessionApiReadGraph = CompilationResult<SessionApiReadState>;

#[must_use]
pub fn session_api_read_decision_label(decision: SessionApiReadDecision) -> &'static str {
    match decision {
        SessionApiReadDecision::Unclassified => "unclassified",
        SessionApiReadDecision::Verified => "verified",
    }
}

#[must_use]
pub fn session_api_read_surface_label(surface: SessionApiReadSurface) -> &'static str {
    match surface {
        SessionApiReadSurface::Summary => "summary",
        SessionApiReadSurface::Detail => "detail",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("session API read JSON value must serialize"),
    ))
}

fn phase_read_count(response: &Value) -> u64 {
    match response.get("phases_read") {
        Some(Value::Array(values)) => {
            values.iter().filter(|value| value.is_string()).count() as u64
        }
        Some(Value::Object(by_skill)) => by_skill
            .values()
            .filter_map(Value::as_array)
            .map(|values| values.iter().filter(|value| value.is_string()).count() as u64)
            .sum(),
        _ => 0,
    }
}

fn proof_chain_count(response: &Value) -> u64 {
    if let Some(count) = response.get("proof_chain_count").and_then(Value::as_u64) {
        return count;
    }
    match response.get("proof_chains") {
        Some(Value::Array(values)) => values.len() as u64,
        Some(Value::Object(values)) => values.len() as u64,
        _ => 0,
    }
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "session_api_read")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn session_api_read_state_schema() -> StateSchema<SessionApiReadState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "surface",
                "session_id",
                "response_sha256",
                "response_len",
                "workflow_authority_present",
                "workflow_authority_langgraph",
                "active",
                "active_skill_present",
                "file_present",
                "tool_calls",
                "phase_read_count",
                "langgraph_workflow_count",
                "langgraph_workflow_key_count",
                "proof_chain_count",
                "hook_stats_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "surface": { "type": "string", "enum": ["Summary", "Detail"] },
                "session_id": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
                        { "type": "null" }
                    ]
                },
                "response_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "response_len": { "type": "integer", "minimum": 1 },
                "workflow_authority_present": { "type": "boolean" },
                "workflow_authority_langgraph": { "type": "boolean" },
                "active": { "type": "boolean" },
                "active_skill_present": { "type": "boolean" },
                "file_present": { "type": "boolean" },
                "tool_calls": { "type": "integer", "minimum": 0 },
                "phase_read_count": { "type": "integer", "minimum": 0 },
                "langgraph_workflow_count": { "type": "integer", "minimum": 0 },
                "langgraph_workflow_key_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "proof_chain_count": { "type": "integer", "minimum": 0 },
                "hook_stats_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Verified"]
                }
            },
            "x-sentinel": {
                "graph": "session_api_read",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SessionApiReadState| {
            if state
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
                .is_none()
            {
                return Err(StateError::ValidationFailed(
                    "session API read response requires session id".to_string(),
                ));
            }
            if state.response_len == 0 || !hex_digest_present(&state.response_sha256) {
                return Err(StateError::ValidationFailed(
                    "session API read response digest must identify a serialized response"
                        .to_string(),
                ));
            }
            if state.workflow_authority_present {
                return Err(StateError::ValidationFailed(
                    "session API read response must not declare workflow authority before read graph audit"
                        .to_string(),
                ));
            }
            if state.surface == SessionApiReadSurface::Summary && !state.file_present {
                return Err(StateError::ValidationFailed(
                    "session API summary response requires source file identity".to_string(),
                ));
            }
            if state.surface == SessionApiReadSurface::Detail
                && state.langgraph_workflow_key_count.is_none()
            {
                return Err(StateError::ValidationFailed(
                    "session API detail response requires LangGraph workflow projection"
                        .to_string(),
                ));
            }
            if state
                .langgraph_workflow_key_count
                .is_some_and(|count| count != state.langgraph_workflow_count)
            {
                return Err(StateError::ValidationFailed(
                    "session API read workflow count must match LangGraph workflow projection"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

async fn classify_node(state: SessionApiReadState) -> Result<SessionApiReadState, NodeError> {
    let mut next = state;
    next.decision = SessionApiReadDecision::Verified;
    Ok(next)
}

async fn terminal_node(state: SessionApiReadState) -> Result<SessionApiReadState, NodeError> {
    let mut next = state;
    next.decision = SessionApiReadDecision::Verified;
    Ok(next)
}

pub async fn build_session_api_read_graph() -> Result<SessionApiReadGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("session_api_read").await?;
    build_session_api_read_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_session_api_read_graph_with_ephemeral_sqlite() -> Result<SessionApiReadGraph, String>
{
    build_session_api_read_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_session_api_read_graph_with_database_path(
    db_path: &str,
) -> Result<SessionApiReadGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_session_api_read_graph_with_checkpointer(checkpointer).await
}

async fn build_session_api_read_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SessionApiReadGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = session_api_read_state_schema();
    let builder = StateGraphBuilder::<SessionApiReadState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: SessionApiReadState| async move {
                emit_decision_node_event("session_api_read", CLASSIFY, &s.identifier)?;
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
            |s: SessionApiReadState| async move {
                emit_decision_node_event("session_api_read", VERIFIED, &s.identifier)?;
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
        .add_conditional_edge(CLASSIFY, |_s: &SessionApiReadState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_session_api_read_decision_report(
    compiled: &SessionApiReadGraph,
    state: SessionApiReadState,
) -> Result<SessionApiReadGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "session_api_read",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "session_api_read", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "session_api_read",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SessionApiReadGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: session_api_read_graph_topology(compiled)?,
    })
}

pub fn session_api_read_graph_topology(
    compiled: &SessionApiReadGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("session_api_read", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_response() -> Value {
        serde_json::json!({
            "id": "summary-session",
            "file": "summary-session.json",
            "started_at": "2026-06-18T12:00:00Z",
            "active": true,
            "active_skill": "linear",
            "tool_calls": 2,
            "phases_read": ["claim.md"],
            "langgraph_workflow_count": 1,
            "proof_chain_count": 0,
            "hook_stats": {
                "total_invocations": 2
            }
        })
    }

    fn detail_response() -> Value {
        serde_json::json!({
            "session_id": "detail-session",
            "started_at": "2026-06-18T12:00:00Z",
            "active": true,
            "active_skill": "linear",
            "tool_calls": 2,
            "phases_read": {
                "linear": ["claim.md"]
            },
            "proof_chains": {},
            "hook_stats": {
                "total_invocations": 2
            },
            "langgraph_workflows": {
                "linear": {
                    "skill": "linear"
                }
            },
            "langgraph_workflow_count": 1,
        })
    }

    #[tokio::test]
    async fn graph_authorizes_session_api_summary_response() {
        let graph = build_session_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SessionApiReadState::from_response(
            SessionApiReadSurface::Summary,
            "summary-session",
            &summary_response(),
        );
        let run = run_session_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SessionApiReadDecision::Verified);
        assert!(run
            .session_api_read_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_session_api_detail_response() {
        let graph = build_session_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SessionApiReadState::from_response(
            SessionApiReadSurface::Detail,
            "detail-session",
            &detail_response(),
        );
        let run = run_session_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SessionApiReadDecision::Verified);
        assert_eq!(run.state.langgraph_workflow_key_count, Some(1));
        assert!(run
            .session_api_read_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_rejects_explicit_workflow_authority_before_read_audit() {
        let graph = build_session_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = summary_response();
        forged["workflow_authority"] = serde_json::json!("langgraph");
        let state = SessionApiReadState::from_response(
            SessionApiReadSurface::Summary,
            "summary-session-forged-authority",
            &forged,
        );
        let err = run_session_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("before read graph audit"), "{err}");
    }

    #[tokio::test]
    async fn graph_rejects_detail_without_langgraph_workflow_projection() {
        let graph = build_session_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = detail_response();
        forged
            .as_object_mut()
            .unwrap()
            .remove("langgraph_workflows");
        let state = SessionApiReadState::from_response(
            SessionApiReadSurface::Detail,
            "detail-session-forged",
            &forged,
        );
        let err = run_session_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(
            err.contains("LangGraph workflow projection"),
            "unexpected error: {err}"
        );
    }
}
