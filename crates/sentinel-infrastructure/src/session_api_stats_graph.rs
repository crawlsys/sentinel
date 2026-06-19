//! Graph-backed audit for the local API aggregate session stats endpoint.
//!
//! `/api/sentinel/stats` summarizes all authenticated session files and their
//! projected LangGraph workflow counts. This graph checkpoints that aggregate
//! response so the local API does not claim LangGraph authority from an
//! uncheckpointed JSON projection.

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
pub enum SessionApiStatsDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionApiStatsState {
    pub identifier: String,
    pub stats_sha256: String,
    pub stats_len: u64,
    pub workflow_authority_langgraph: bool,
    pub total_sessions: Option<u64>,
    pub active_sessions: Option<u64>,
    pub total_langgraph_workflows: Option<u64>,
    pub sessions_with_langgraph_workflows: Option<u64>,
    pub total_proof_chains: Option<u64>,
    pub total_hook_invocations: Option<u64>,
    pub total_blocked: Option<u64>,
    pub hook_avg_count: Option<u64>,
    pub skill_usage_count: Option<u64>,
    pub decision: SessionApiStatsDecision,
}

impl SessionApiStatsState {
    #[must_use]
    pub fn from_response(identifier: impl Into<String>, response: &Value) -> Self {
        let stats_bytes =
            serde_json::to_vec(response).expect("serde_json::Value serialization cannot fail");
        Self {
            identifier: identifier.into(),
            stats_sha256: hex::encode(Sha256::digest(&stats_bytes)),
            stats_len: stats_bytes.len() as u64,
            workflow_authority_langgraph: response
                .get("workflow_authority")
                .and_then(Value::as_str)
                == Some("langgraph"),
            total_sessions: response.get("total_sessions").and_then(Value::as_u64),
            active_sessions: response.get("active_sessions").and_then(Value::as_u64),
            total_langgraph_workflows: response
                .get("total_langgraph_workflows")
                .and_then(Value::as_u64),
            sessions_with_langgraph_workflows: response
                .get("sessions_with_langgraph_workflows")
                .and_then(Value::as_u64),
            total_proof_chains: response.get("total_proof_chains").and_then(Value::as_u64),
            total_hook_invocations: response
                .get("total_hook_invocations")
                .and_then(Value::as_u64),
            total_blocked: response.get("total_blocked").and_then(Value::as_u64),
            hook_avg_count: response
                .get("hook_avg_ms")
                .and_then(Value::as_object)
                .map(|hooks| hooks.len() as u64),
            skill_usage_count: response
                .get("skill_usage")
                .and_then(Value::as_object)
                .map(|skills| skills.len() as u64),
            decision: SessionApiStatsDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionApiStatsGraphRun {
    pub state: SessionApiStatsState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SessionApiStatsState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct SessionApiStatsAuthorization {
    decision: SessionApiStatsDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl SessionApiStatsAuthorization {
    #[must_use]
    pub fn decision(&self) -> SessionApiStatsDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl SessionApiStatsGraphRun {
    #[must_use]
    pub fn session_api_stats_authorization(
        &self,
    ) -> Result<Option<SessionApiStatsAuthorization>, String> {
        if self.state.decision == SessionApiStatsDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "session_api_stats",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SessionApiStatsAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type SessionApiStatsGraph = CompilationResult<SessionApiStatsState>;

#[must_use]
pub fn session_api_stats_decision_label(decision: SessionApiStatsDecision) -> &'static str {
    match decision {
        SessionApiStatsDecision::Unclassified => "unclassified",
        SessionApiStatsDecision::Verified => "verified",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("serde_json::Value serialization cannot fail"),
    ))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "session_api_stats")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn session_api_stats_state_schema() -> StateSchema<SessionApiStatsState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "stats_sha256",
                "stats_len",
                "workflow_authority_langgraph",
                "total_sessions",
                "active_sessions",
                "total_langgraph_workflows",
                "sessions_with_langgraph_workflows",
                "total_proof_chains",
                "total_hook_invocations",
                "total_blocked",
                "hook_avg_count",
                "skill_usage_count",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "stats_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "stats_len": { "type": "integer", "minimum": 1 },
                "workflow_authority_langgraph": { "type": "boolean" },
                "total_sessions": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "active_sessions": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "total_langgraph_workflows": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "sessions_with_langgraph_workflows": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "total_proof_chains": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "total_hook_invocations": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "total_blocked": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "hook_avg_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "skill_usage_count": {
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
                "graph": "session_api_stats",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SessionApiStatsState| {
            if state.stats_len == 0 || !hex_digest_present(&state.stats_sha256) {
                return Err(StateError::ValidationFailed(
                    "session API stats digest must identify a serialized response".to_string(),
                ));
            }
            if !state.workflow_authority_langgraph {
                return Err(StateError::ValidationFailed(
                    "session API stats response must declare LangGraph workflow authority"
                        .to_string(),
                ));
            }
            let require_count = |value: Option<u64>, field: &str| -> Result<u64, StateError> {
                value.ok_or_else(|| {
                    StateError::ValidationFailed(format!(
                        "session API stats response requires numeric {field}"
                    ))
                })
            };
            let total_sessions = require_count(state.total_sessions, "total_sessions")?;
            let active_sessions = require_count(state.active_sessions, "active_sessions")?;
            let total_langgraph_workflows =
                require_count(state.total_langgraph_workflows, "total_langgraph_workflows")?;
            let sessions_with_langgraph_workflows = require_count(
                state.sessions_with_langgraph_workflows,
                "sessions_with_langgraph_workflows",
            )?;
            let total_proof_chains =
                require_count(state.total_proof_chains, "total_proof_chains")?;
            let total_hook_invocations =
                require_count(state.total_hook_invocations, "total_hook_invocations")?;
            let total_blocked = require_count(state.total_blocked, "total_blocked")?;
            let _hook_avg_count = require_count(state.hook_avg_count, "hook_avg_ms")?;
            let _skill_usage_count = require_count(state.skill_usage_count, "skill_usage")?;

            if active_sessions > total_sessions {
                return Err(StateError::ValidationFailed(
                    "session API stats active session count cannot exceed total sessions"
                        .to_string(),
                ));
            }
            if sessions_with_langgraph_workflows > total_sessions {
                return Err(StateError::ValidationFailed(
                    "session API stats LangGraph workflow session count cannot exceed total sessions"
                        .to_string(),
                ));
            }
            if total_blocked > total_hook_invocations {
                return Err(StateError::ValidationFailed(
                    "session API stats blocked count cannot exceed invocation count".to_string(),
                ));
            }
            if total_sessions == 0
                && (active_sessions != 0
                    || total_langgraph_workflows != 0
                    || sessions_with_langgraph_workflows != 0
                    || total_proof_chains != 0
                    || total_hook_invocations != 0
                    || total_blocked != 0)
            {
                return Err(StateError::ValidationFailed(
                    "session API stats with no sessions must not report session-derived counts"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

async fn classify_node(state: SessionApiStatsState) -> Result<SessionApiStatsState, NodeError> {
    let mut next = state;
    next.decision = SessionApiStatsDecision::Verified;
    Ok(next)
}

async fn terminal_node(state: SessionApiStatsState) -> Result<SessionApiStatsState, NodeError> {
    let mut next = state;
    next.decision = SessionApiStatsDecision::Verified;
    Ok(next)
}

pub async fn build_session_api_stats_graph() -> Result<SessionApiStatsGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("session_api_stats").await?;
    build_session_api_stats_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_session_api_stats_graph_with_ephemeral_sqlite(
) -> Result<SessionApiStatsGraph, String> {
    build_session_api_stats_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_session_api_stats_graph_with_database_path(
    db_path: &str,
) -> Result<SessionApiStatsGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_session_api_stats_graph_with_checkpointer(checkpointer).await
}

async fn build_session_api_stats_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SessionApiStatsGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = session_api_stats_state_schema();
    let builder = StateGraphBuilder::<SessionApiStatsState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: SessionApiStatsState| async move {
                emit_decision_node_event("session_api_stats", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            VERIFIED,
            |s: SessionApiStatsState| async move {
                emit_decision_node_event("session_api_stats", VERIFIED, &s.identifier)?;
                terminal_node(s).await
            },
            node_config(VERIFIED, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |_s: &SessionApiStatsState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_session_api_stats_decision_report(
    compiled: &SessionApiStatsGraph,
    state: SessionApiStatsState,
) -> Result<SessionApiStatsGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "session_api_stats",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "session_api_stats",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "session_api_stats",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SessionApiStatsGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: session_api_stats_graph_topology(compiled)?,
    })
}

pub fn session_api_stats_graph_topology(
    compiled: &SessionApiStatsGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("session_api_stats", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Value {
        serde_json::json!({
            "total_sessions": 2,
            "active_sessions": 1,
            "workflow_authority": "langgraph",
            "total_langgraph_workflows": 3,
            "sessions_with_langgraph_workflows": 2,
            "total_proof_chains": 2,
            "total_hook_invocations": 10,
            "total_blocked": 1,
            "hook_avg_ms": {
                "PreToolUse": 15
            },
            "skill_usage": {
                "linear": 2
            }
        })
    }

    #[tokio::test]
    async fn graph_authorizes_session_api_stats_response() {
        let graph = build_session_api_stats_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SessionApiStatsState::from_response("api-stats", &response());
        let run = run_session_api_stats_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SessionApiStatsDecision::Verified);
        assert!(run
            .session_api_stats_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_schema_rejects_untrusted_authority() {
        let graph = build_session_api_stats_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = response();
        forged["workflow_authority"] = serde_json::json!("local");
        let state = SessionApiStatsState::from_response("api-stats-forged", &forged);
        let err = run_session_api_stats_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("LangGraph workflow authority"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_numeric_stats_fields() {
        let graph = build_session_api_stats_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = response();
        forged
            .as_object_mut()
            .unwrap()
            .remove("total_hook_invocations");
        let state = SessionApiStatsState::from_response("api-stats-missing-count", &forged);
        let err = run_session_api_stats_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("total_hook_invocations"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_hook_average_object() {
        let graph = build_session_api_stats_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = response();
        forged.as_object_mut().unwrap().remove("hook_avg_ms");
        let state = SessionApiStatsState::from_response("api-stats-missing-hook-avg", &forged);
        let err = run_session_api_stats_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("hook_avg_ms"), "{err}");
    }
}
