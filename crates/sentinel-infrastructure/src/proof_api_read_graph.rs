//! Graph-backed audit for local API proof read responses.
//!
//! Proof API responses are only authoritative when paired with
//! LangGraph-projected workflow state. This graph checkpoints each rendered API
//! proof response so that the API boundary itself carries durable LangGraph
//! evidence before callers stamp the `workflow_authority = "langgraph"` claim.

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
pub enum ProofApiReadSurface {
    List,
    Summary,
    Chain,
    Verify,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProofApiReadDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofApiReadState {
    pub identifier: String,
    pub surface: ProofApiReadSurface,
    pub response_sha256: String,
    pub response_len: u64,
    pub workflow_authority_present: bool,
    pub workflow_authority_langgraph: bool,
    pub chains_count: Option<u64>,
    pub child_authority_count: u64,
    pub child_graph_audit_count: u64,
    pub skill_present: bool,
    pub session_id_present: bool,
    pub error_present: bool,
    pub graph_workflow_present: bool,
    pub entries_count: Option<u64>,
    pub valid_present: bool,
    pub signatures_present: bool,
    pub decision: ProofApiReadDecision,
}

impl ProofApiReadState {
    #[must_use]
    pub fn from_response(
        surface: ProofApiReadSurface,
        identifier: impl Into<String>,
        response: &Value,
    ) -> Self {
        let response_bytes =
            serde_json::to_vec(response).expect("proof API read response must serialize");
        let chains = response.get("chains").and_then(Value::as_array);
        Self {
            identifier: identifier.into(),
            surface,
            response_sha256: hex::encode(Sha256::digest(&response_bytes)),
            response_len: response_bytes.len() as u64,
            workflow_authority_present: response.get("workflow_authority").is_some(),
            workflow_authority_langgraph: response
                .get("workflow_authority")
                .and_then(Value::as_str)
                == Some("langgraph"),
            chains_count: chains.map(|items| items.len() as u64),
            child_authority_count: chains.map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("workflow_authority").and_then(Value::as_str) == Some("langgraph")
                    })
                    .count() as u64
            }),
            child_graph_audit_count: chains.map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| item.get("graph_audit").is_some())
                    .count() as u64
            }),
            skill_present: response
                .get("skill")
                .and_then(Value::as_str)
                .is_some_and(|skill| !skill.trim().is_empty()),
            session_id_present: response
                .get("session_id")
                .and_then(Value::as_str)
                .is_some_and(|session_id| !session_id.trim().is_empty()),
            error_present: response
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.trim().is_empty()),
            graph_workflow_present: response.get("graph_workflow").is_some(),
            entries_count: response
                .get("entries")
                .and_then(Value::as_array)
                .map(|entries| entries.len() as u64),
            valid_present: response.get("valid").and_then(Value::as_bool).is_some(),
            signatures_present: response.get("signatures").is_some(),
            decision: ProofApiReadDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProofApiReadGraphRun {
    pub state: ProofApiReadState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<ProofApiReadState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct ProofApiReadAuthorization {
    decision: ProofApiReadDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl ProofApiReadAuthorization {
    #[must_use]
    pub fn decision(&self) -> ProofApiReadDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl ProofApiReadGraphRun {
    #[must_use]
    pub fn proof_api_read_authorization(
        &self,
    ) -> Result<Option<ProofApiReadAuthorization>, String> {
        if self.state.decision == ProofApiReadDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "proof_api_read",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(ProofApiReadAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type ProofApiReadGraph = CompilationResult<ProofApiReadState>;

#[must_use]
pub fn proof_api_read_decision_label(decision: ProofApiReadDecision) -> &'static str {
    match decision {
        ProofApiReadDecision::Unclassified => "unclassified",
        ProofApiReadDecision::Verified => "verified",
    }
}

#[must_use]
pub fn proof_api_read_surface_label(surface: ProofApiReadSurface) -> &'static str {
    match surface {
        ProofApiReadSurface::List => "list",
        ProofApiReadSurface::Summary => "summary",
        ProofApiReadSurface::Chain => "chain",
        ProofApiReadSurface::Verify => "verify",
        ProofApiReadSurface::Error => "error",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("proof API read JSON value must serialize"),
    ))
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
        .with_metadata("sentinel.graph", "proof_api_read")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn proof_api_read_state_schema() -> StateSchema<ProofApiReadState> {
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
                "workflow_authority_present",
                "workflow_authority_langgraph",
                "chains_count",
                "child_authority_count",
                "child_graph_audit_count",
                "skill_present",
                "session_id_present",
                "error_present",
                "graph_workflow_present",
                "entries_count",
                "valid_present",
                "signatures_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "surface": {
                    "type": "string",
                    "enum": ["List", "Summary", "Chain", "Verify", "Error"]
                },
                "response_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "response_len": { "type": "integer", "minimum": 1 },
                "workflow_authority_present": { "type": "boolean" },
                "workflow_authority_langgraph": { "type": "boolean" },
                "chains_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "child_authority_count": { "type": "integer", "minimum": 0 },
                "child_graph_audit_count": { "type": "integer", "minimum": 0 },
                "skill_present": { "type": "boolean" },
                "session_id_present": { "type": "boolean" },
                "error_present": { "type": "boolean" },
                "graph_workflow_present": { "type": "boolean" },
                "entries_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "valid_present": { "type": "boolean" },
                "signatures_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Verified"]
                }
            },
            "x-sentinel": {
                "graph": "proof_api_read",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &ProofApiReadState| {
            if state.response_len == 0 || !hex_digest_present(&state.response_sha256) {
                return Err(StateError::ValidationFailed(
                    "proof API response digest must identify a serialized response".to_string(),
                ));
            }
            if state.workflow_authority_present {
                return Err(StateError::ValidationFailed(
                    "proof API response must not declare workflow authority before read graph audit"
                        .to_string(),
                ));
            }
            match state.surface {
                ProofApiReadSurface::List => {
                    let Some(count) = state.chains_count else {
                        return Err(StateError::ValidationFailed(
                            "proof API list response requires chains array".to_string(),
                        ));
                    };
                    if state.child_authority_count != count {
                        return Err(StateError::ValidationFailed(
                            "proof API list response requires all chain summaries to declare LangGraph authority"
                                .to_string(),
                        ));
                    }
                    if state.child_graph_audit_count != count {
                        return Err(StateError::ValidationFailed(
                            "proof API list response requires all chain summaries to carry graph audit"
                                .to_string(),
                        ));
                    }
                }
                ProofApiReadSurface::Summary => {
                    if !state.skill_present || !state.session_id_present {
                        return Err(StateError::ValidationFailed(
                            "proof API summary response requires skill and session id".to_string(),
                        ));
                    }
                    if !state.graph_workflow_present {
                        return Err(StateError::ValidationFailed(
                            "proof API summary response requires LangGraph workflow projection"
                                .to_string(),
                        ));
                    }
                }
                ProofApiReadSurface::Chain => {
                    if !state.skill_present || !state.session_id_present {
                        return Err(StateError::ValidationFailed(
                            "proof API chain response requires skill and session id".to_string(),
                        ));
                    }
                    if !state.graph_workflow_present {
                        return Err(StateError::ValidationFailed(
                            "proof API chain response requires LangGraph workflow projection"
                                .to_string(),
                        ));
                    }
                    if state.entries_count.is_none() {
                        return Err(StateError::ValidationFailed(
                            "proof API chain response requires canonical entries".to_string(),
                        ));
                    }
                }
                ProofApiReadSurface::Verify => {
                    if !state.graph_workflow_present {
                        return Err(StateError::ValidationFailed(
                            "proof API verification response requires LangGraph workflow projection"
                                .to_string(),
                        ));
                    }
                    if !state.valid_present || !state.signatures_present {
                        return Err(StateError::ValidationFailed(
                            "proof API verification response requires validity and signature report"
                                .to_string(),
                        ));
                    }
                }
                ProofApiReadSurface::Error => {
                    if !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "proof API error response requires error text".to_string(),
                        ));
                    }
                }
            }
            Ok(())
        })
}

async fn classify_node(state: ProofApiReadState) -> Result<ProofApiReadState, NodeError> {
    let mut next = state;
    next.decision = ProofApiReadDecision::Verified;
    Ok(next)
}

async fn terminal_node(state: ProofApiReadState) -> Result<ProofApiReadState, NodeError> {
    let mut next = state;
    next.decision = ProofApiReadDecision::Verified;
    Ok(next)
}

pub async fn build_proof_api_read_graph() -> Result<ProofApiReadGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("proof_api_read").await?;
    build_proof_api_read_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_proof_api_read_graph_with_ephemeral_sqlite() -> Result<ProofApiReadGraph, String> {
    build_proof_api_read_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_proof_api_read_graph_with_database_path(
    db_path: &str,
) -> Result<ProofApiReadGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_proof_api_read_graph_with_checkpointer(checkpointer).await
}

async fn build_proof_api_read_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<ProofApiReadGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = proof_api_read_state_schema();
    let builder = StateGraphBuilder::<ProofApiReadState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: ProofApiReadState| async move {
                emit_decision_node_event("proof_api_read", CLASSIFY, &s.identifier)?;
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
            VERIFIED,
            |s: ProofApiReadState| async move {
                emit_decision_node_event("proof_api_read", VERIFIED, &s.identifier)?;
                terminal_node(s).await
            },
            node_config(
                VERIFIED,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |_s: &ProofApiReadState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_proof_api_read_decision_report(
    compiled: &ProofApiReadGraph,
    state: ProofApiReadState,
) -> Result<ProofApiReadGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "proof_api_read",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "proof_api_read", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "proof_api_read",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(ProofApiReadGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: proof_api_read_graph_topology(compiled)?,
    })
}

pub fn proof_api_read_graph_topology(
    compiled: &ProofApiReadGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("proof_api_read", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary_response() -> Value {
        serde_json::json!({
            "skill": "linear",
            "session_id": "sess",
            "phases": 1,
            "complete": false,
            "chain_valid": true,
            "graph_workflow": {
                "skill": "linear",
                "session_id": "sess"
            }
        })
    }

    fn audited_summary_response() -> Value {
        let mut summary = summary_response();
        summary.as_object_mut().unwrap().insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
        summary.as_object_mut().unwrap().insert(
            "graph_audit".to_string(),
            serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "proof_api_read"
            }),
        );
        summary
    }

    fn chain_response() -> Value {
        serde_json::json!({
            "skill": "linear",
            "session_id": "sess",
            "genesis_hash": "0000000000000000000000000000000000000000000000000000000000000000",
            "proofs": [],
            "entries": [],
            "complete": false,
            "chain_valid": true,
            "graph_workflow": {
                "skill": "linear",
                "session_id": "sess"
            }
        })
    }

    fn verify_response() -> Value {
        serde_json::json!({
            "valid": true,
            "phases_verified": 0,
            "steps_verified": 1,
            "errors": [],
            "signatures": {
                "checked": true,
                "required": true,
                "verified": 1,
                "unsigned": 0,
                "failures": []
            },
            "graph_workflow": {
                "skill": "linear",
                "session_id": "sess"
            }
        })
    }

    #[tokio::test]
    async fn graph_authorizes_proof_summary_response() {
        let graph = build_proof_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProofApiReadState::from_response(
            ProofApiReadSurface::Summary,
            "summary-linear",
            &summary_response(),
        );
        let run = run_proof_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, ProofApiReadDecision::Verified);
        assert!(!run.state.workflow_authority_present);
        assert!(run
            .proof_api_read_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_proof_list_response_with_audited_children() {
        let graph = build_proof_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = serde_json::json!({
            "chains": [audited_summary_response()]
        });
        let state = ProofApiReadState::from_response(ProofApiReadSurface::List, "list", &response);
        let run = run_proof_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, ProofApiReadDecision::Verified);
        assert_eq!(run.state.child_authority_count, 1);
        assert_eq!(run.state.child_graph_audit_count, 1);
    }

    #[tokio::test]
    async fn graph_authorizes_proof_chain_and_verify_responses() {
        let graph = build_proof_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let chain_state = ProofApiReadState::from_response(
            ProofApiReadSurface::Chain,
            "chain-sess",
            &chain_response(),
        );
        let verify_state = ProofApiReadState::from_response(
            ProofApiReadSurface::Verify,
            "verify-sess",
            &verify_response(),
        );

        let chain_run = run_proof_api_read_decision_report(&graph, chain_state)
            .await
            .unwrap();
        let verify_run = run_proof_api_read_decision_report(&graph, verify_state)
            .await
            .unwrap();

        assert_eq!(chain_run.state.decision, ProofApiReadDecision::Verified);
        assert_eq!(verify_run.state.decision, ProofApiReadDecision::Verified);
    }

    #[tokio::test]
    async fn graph_rejects_proof_list_with_unaudited_children() {
        let graph = build_proof_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut child = summary_response();
        child.as_object_mut().unwrap().insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
        let response = serde_json::json!({
            "chains": [child]
        });
        let state =
            ProofApiReadState::from_response(ProofApiReadSurface::List, "list-forged", &response);
        let err = run_proof_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("graph audit"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn graph_rejects_explicit_workflow_authority_before_read_audit() {
        let graph = build_proof_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut response = summary_response();
        response.as_object_mut().unwrap().insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
        let state =
            ProofApiReadState::from_response(ProofApiReadSurface::Summary, "forged", &response);
        let err = run_proof_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(
            err.contains("before read graph audit"),
            "unexpected error: {err}"
        );
    }
}
