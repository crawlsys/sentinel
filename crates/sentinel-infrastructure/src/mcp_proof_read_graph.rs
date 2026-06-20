//! Graph-backed audit for MCP proof and step read responses.
//!
//! Proof chains are already hash-verifiable, but MCP read responses are still
//! an external authority boundary. This graph checkpoints each rendered proof
//! read response so the MCP boundary itself carries durable LangGraph evidence
//! before callers stamp `workflow_authority = "langgraph"` onto the response.

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
pub enum McpProofReadSurface {
    ProofChain,
    WorkflowStatus,
    VerifyChain,
    StepProof,
    StepChain,
    ActiveStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum McpProofReadDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpProofReadState {
    pub identifier: String,
    pub surface: McpProofReadSurface,
    pub response_sha256: String,
    pub response_len: u64,
    pub workflow_authority_present: bool,
    pub workflow_authority_langgraph: bool,
    pub graph_workflow_present: bool,
    pub skill_present: bool,
    pub session_id_present: bool,
    pub entries_count: Option<u64>,
    pub steps_count: Option<u64>,
    pub workflow_status_fields_present: bool,
    pub verification_valid_present: bool,
    pub verification_counts_present: bool,
    pub verification_errors_present: bool,
    pub step_id_present: bool,
    pub phase_id_present: bool,
    pub combined_hash_present: bool,
    pub head_hash_present: bool,
    pub chain_length_present: bool,
    pub last_step_field_present: bool,
    pub decision: McpProofReadDecision,
}

impl McpProofReadState {
    #[must_use]
    pub fn from_response(
        surface: McpProofReadSurface,
        identifier: impl Into<String>,
        response: &Value,
    ) -> Self {
        let response_bytes =
            serde_json::to_vec(response).expect("MCP proof read response must serialize");
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
            graph_workflow_present: response.get("graph_workflow").is_some(),
            skill_present: non_empty_string(response, "skill"),
            session_id_present: non_empty_string(response, "session_id"),
            entries_count: response
                .get("entries")
                .and_then(Value::as_array)
                .map(|items| items.len() as u64),
            steps_count: response
                .get("steps")
                .and_then(Value::as_array)
                .map(|items| items.len() as u64),
            workflow_status_fields_present: response.get("current_phase").is_some()
                && response
                    .get("completed_phases")
                    .and_then(Value::as_array)
                    .is_some()
                && response.get("complete").and_then(Value::as_bool).is_some(),
            verification_valid_present: response.get("valid").and_then(Value::as_bool).is_some(),
            verification_counts_present: response
                .get("phases_verified")
                .and_then(Value::as_u64)
                .is_some()
                && response
                    .get("steps_verified")
                    .and_then(Value::as_u64)
                    .is_some(),
            verification_errors_present: response.get("errors").and_then(Value::as_array).is_some(),
            step_id_present: non_empty_string(response, "step_id"),
            phase_id_present: non_empty_string(response, "phase_id"),
            combined_hash_present: response
                .get("combined_hash")
                .and_then(Value::as_str)
                .is_some_and(hex_digest_present),
            head_hash_present: response
                .get("head_hash")
                .and_then(Value::as_str)
                .is_some_and(hex_digest_present),
            chain_length_present: response
                .get("chain_length")
                .and_then(Value::as_u64)
                .is_some(),
            last_step_field_present: response.get("last_step").is_some(),
            decision: McpProofReadDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct McpProofReadGraphRun {
    pub state: McpProofReadState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<McpProofReadState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct McpProofReadAuthorization {
    decision: McpProofReadDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl McpProofReadAuthorization {
    #[must_use]
    pub fn decision(&self) -> McpProofReadDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl McpProofReadGraphRun {
    pub fn mcp_proof_read_authorization(
        &self,
    ) -> Result<Option<McpProofReadAuthorization>, String> {
        if self.state.decision == McpProofReadDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "mcp_proof_read",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(McpProofReadAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type McpProofReadGraph = CompilationResult<McpProofReadState>;

#[must_use]
pub fn mcp_proof_read_decision_label(decision: McpProofReadDecision) -> &'static str {
    match decision {
        McpProofReadDecision::Unclassified => "unclassified",
        McpProofReadDecision::Verified => "verified",
    }
}

#[must_use]
pub fn mcp_proof_read_surface_label(surface: McpProofReadSurface) -> &'static str {
    match surface {
        McpProofReadSurface::ProofChain => "proof_chain",
        McpProofReadSurface::WorkflowStatus => "workflow_status",
        McpProofReadSurface::VerifyChain => "verify_chain",
        McpProofReadSurface::StepProof => "step_proof",
        McpProofReadSurface::StepChain => "step_chain",
        McpProofReadSurface::ActiveStep => "active_step",
    }
}

#[must_use]
pub fn mcp_proof_read_identifier(
    surface: McpProofReadSurface,
    response: &Value,
    response_hash: &str,
) -> Result<String, String> {
    let skill = required_response_string(response, "skill")?;
    let session_id = required_response_string(response, "session_id")?;
    let surface_anchor = match surface {
        McpProofReadSurface::ProofChain => {
            let entries_count = required_array_len(response, "entries")?;
            format!("entries-{entries_count}")
        }
        McpProofReadSurface::WorkflowStatus => {
            require_field(response, "current_phase")?;
            let completed_count = required_array_len(response, "completed_phases")?;
            let complete = required_bool(response, "complete")?;
            let current_phase = response
                .get("current_phase")
                .and_then(Value::as_u64)
                .map_or_else(|| "none".to_string(), |phase| phase.to_string());
            format!("current-phase-{current_phase}:completed-{completed_count}:complete-{complete}")
        }
        McpProofReadSurface::VerifyChain => {
            let valid = required_bool(response, "valid")?;
            let phases_verified = required_u64(response, "phases_verified")?;
            let steps_verified = required_u64(response, "steps_verified")?;
            let errors_count = required_array_len(response, "errors")?;
            format!(
                "valid-{valid}:phases-{phases_verified}:steps-{steps_verified}:errors-{errors_count}"
            )
        }
        McpProofReadSurface::StepProof => {
            let phase_id = required_response_string(response, "phase_id")?;
            let step_id = required_response_string(response, "step_id")?;
            let combined_hash = required_hex_digest(response, "combined_hash")?;
            format!("phase-{phase_id}:step-{step_id}:combined-{combined_hash}")
        }
        McpProofReadSurface::StepChain => {
            let steps_count = required_array_len(response, "steps")?;
            let head_hash = required_hex_digest(response, "head_hash")?;
            format!("head-{head_hash}:steps-{steps_count}")
        }
        McpProofReadSurface::ActiveStep => {
            require_field(response, "last_step")?;
            let chain_length = required_u64(response, "chain_length")?;
            let head_hash = required_hex_digest(response, "head_hash")?;
            format!("head-{head_hash}:chain-length-{chain_length}")
        }
    };
    Ok(format!(
        "{}-{skill}-{session_id}:{surface_anchor}:response-{response_hash}",
        mcp_proof_read_surface_label(surface)
    ))
}

fn required_response_string<'a>(response: &'a Value, field: &str) -> Result<&'a str, String> {
    response
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("MCP proof read response requires concrete {field}"))
}

fn required_hex_digest<'a>(response: &'a Value, field: &str) -> Result<&'a str, String> {
    let value = required_response_string(response, field)?;
    if hex_digest_present(value) {
        Ok(value)
    } else {
        Err(format!(
            "MCP proof read response requires {field} to be a 64-character hex digest"
        ))
    }
}

fn required_array_len(response: &Value, field: &str) -> Result<usize, String> {
    response
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| format!("MCP proof read response requires canonical {field} array"))
}

fn required_u64(response: &Value, field: &str) -> Result<u64, String> {
    response
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("MCP proof read response requires concrete numeric {field}"))
}

fn required_bool(response: &Value, field: &str) -> Result<bool, String> {
    response
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("MCP proof read response requires concrete boolean {field}"))
}

fn require_field(response: &Value, field: &str) -> Result<(), String> {
    if response.get(field).is_some() {
        Ok(())
    } else {
        Err(format!("MCP proof read response requires {field} field"))
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("MCP proof read JSON value must serialize"),
    ))
}

fn non_empty_string(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
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
        .with_metadata("sentinel.graph", "mcp_proof_read")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn mcp_proof_read_state_schema() -> StateSchema<McpProofReadState> {
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
                "graph_workflow_present",
                "skill_present",
                "session_id_present",
                "entries_count",
                "steps_count",
                "workflow_status_fields_present",
                "verification_valid_present",
                "verification_counts_present",
                "verification_errors_present",
                "step_id_present",
                "phase_id_present",
                "combined_hash_present",
                "head_hash_present",
                "chain_length_present",
                "last_step_field_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "surface": {
                    "type": "string",
                    "enum": [
                        "ProofChain",
                        "WorkflowStatus",
                        "VerifyChain",
                        "StepProof",
                        "StepChain",
                        "ActiveStep"
                    ]
                },
                "response_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "response_len": { "type": "integer", "minimum": 1 },
                "workflow_authority_present": { "type": "boolean" },
                "workflow_authority_langgraph": { "type": "boolean" },
                "graph_workflow_present": { "type": "boolean" },
                "skill_present": { "type": "boolean" },
                "session_id_present": { "type": "boolean" },
                "entries_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "steps_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "workflow_status_fields_present": { "type": "boolean" },
                "verification_valid_present": { "type": "boolean" },
                "verification_counts_present": { "type": "boolean" },
                "verification_errors_present": { "type": "boolean" },
                "step_id_present": { "type": "boolean" },
                "phase_id_present": { "type": "boolean" },
                "combined_hash_present": { "type": "boolean" },
                "head_hash_present": { "type": "boolean" },
                "chain_length_present": { "type": "boolean" },
                "last_step_field_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Verified"]
                }
            },
            "x-sentinel": {
                "graph": "mcp_proof_read",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &McpProofReadState| {
            if state.response_len == 0 || !hex_digest_present(&state.response_sha256) {
                return Err(StateError::ValidationFailed(
                    "MCP proof read response digest must identify a serialized response"
                        .to_string(),
                ));
            }
            if state.workflow_authority_present {
                return Err(StateError::ValidationFailed(
                    "MCP proof read response must not declare workflow authority before read graph audit"
                        .to_string(),
                ));
            }
            if !state.graph_workflow_present {
                return Err(StateError::ValidationFailed(
                    "MCP proof read response requires LangGraph workflow projection".to_string(),
                ));
            }
            if !state.skill_present || !state.session_id_present {
                return Err(StateError::ValidationFailed(
                    "MCP proof read response requires skill and session id".to_string(),
                ));
            }
            match state.surface {
                McpProofReadSurface::ProofChain => {
                    if state.entries_count.is_none() {
                        return Err(StateError::ValidationFailed(
                            "MCP proof chain read requires canonical entries".to_string(),
                        ));
                    }
                }
                McpProofReadSurface::WorkflowStatus => {
                    if !state.workflow_status_fields_present {
                        return Err(StateError::ValidationFailed(
                            "MCP workflow status read requires workflow status fields".to_string(),
                        ));
                    }
                }
                McpProofReadSurface::VerifyChain => {
                    if !state.verification_valid_present
                        || !state.verification_counts_present
                        || !state.verification_errors_present
                    {
                        return Err(StateError::ValidationFailed(
                            "MCP chain verification read requires verification result fields"
                                .to_string(),
                        ));
                    }
                }
                McpProofReadSurface::StepProof => {
                    if !state.step_id_present
                        || !state.phase_id_present
                        || !state.combined_hash_present
                    {
                        return Err(StateError::ValidationFailed(
                            "MCP step proof read requires step identity and combined hash"
                                .to_string(),
                        ));
                    }
                }
                McpProofReadSurface::StepChain => {
                    if state.steps_count.is_none() || !state.head_hash_present {
                        return Err(StateError::ValidationFailed(
                            "MCP step chain read requires steps and chain head".to_string(),
                        ));
                    }
                }
                McpProofReadSurface::ActiveStep => {
                    if !state.last_step_field_present
                        || !state.chain_length_present
                        || !state.head_hash_present
                    {
                        return Err(StateError::ValidationFailed(
                            "MCP active step read requires last_step, chain_length, and head_hash"
                                .to_string(),
                        ));
                    }
                }
            }
            Ok(())
        })
}

async fn classify_node(state: McpProofReadState) -> Result<McpProofReadState, NodeError> {
    let mut next = state;
    next.decision = McpProofReadDecision::Verified;
    Ok(next)
}

async fn terminal_node(state: McpProofReadState) -> Result<McpProofReadState, NodeError> {
    let mut next = state;
    next.decision = McpProofReadDecision::Verified;
    Ok(next)
}

pub async fn build_mcp_proof_read_graph() -> Result<McpProofReadGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("mcp_proof_read").await?;
    build_mcp_proof_read_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_mcp_proof_read_graph_with_ephemeral_sqlite() -> Result<McpProofReadGraph, String> {
    build_mcp_proof_read_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_mcp_proof_read_graph_with_database_path(
    db_path: &str,
) -> Result<McpProofReadGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_mcp_proof_read_graph_with_checkpointer(checkpointer).await
}

async fn build_mcp_proof_read_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<McpProofReadGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = mcp_proof_read_state_schema();
    let builder = StateGraphBuilder::<McpProofReadState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: McpProofReadState| async move {
                emit_decision_node_event("mcp_proof_read", CLASSIFY, &s.identifier)?;
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
            |s: McpProofReadState| async move {
                emit_decision_node_event("mcp_proof_read", VERIFIED, &s.identifier)?;
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
        .add_conditional_edge(CLASSIFY, |_s: &McpProofReadState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_mcp_proof_read_decision_report(
    compiled: &McpProofReadGraph,
    state: McpProofReadState,
) -> Result<McpProofReadGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "mcp_proof_read",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "mcp_proof_read", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "mcp_proof_read",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(McpProofReadGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: mcp_proof_read_graph_topology(compiled)?,
    })
}

pub fn mcp_proof_read_graph_topology(
    compiled: &McpProofReadGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("mcp_proof_read", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn base_response(extra: serde_json::Value) -> Value {
        let mut response = serde_json::json!({
            "skill": "linear",
            "session_id": "sess",
            "graph_workflow": {
                "skill": "linear",
                "session_id": "sess"
            }
        });
        response
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        response
    }

    #[test]
    fn proof_read_identifier_uses_surface_specific_evidence() {
        let proof_chain = base_response(serde_json::json!({"entries": []}));
        let proof_chain_hash = sha256_json(&proof_chain);
        let proof_chain_identifier = mcp_proof_read_identifier(
            McpProofReadSurface::ProofChain,
            &proof_chain,
            &proof_chain_hash,
        )
        .unwrap();
        assert!(proof_chain_identifier.contains("proof_chain-linear-sess:entries-0"));
        assert!(!proof_chain_identifier.contains("response-response"));

        let step_chain = base_response(serde_json::json!({
            "steps": [],
            "head_hash": HASH
        }));
        let step_chain_hash = sha256_json(&step_chain);
        let step_chain_identifier = mcp_proof_read_identifier(
            McpProofReadSurface::StepChain,
            &step_chain,
            &step_chain_hash,
        )
        .unwrap();
        assert!(
            step_chain_identifier.contains(&format!("step_chain-linear-sess:head-{HASH}:steps-0"))
        );

        let workflow_status = base_response(serde_json::json!({
            "current_phase": 0,
            "completed_phases": [],
            "complete": false
        }));
        let workflow_status_hash = sha256_json(&workflow_status);
        let workflow_status_identifier = mcp_proof_read_identifier(
            McpProofReadSurface::WorkflowStatus,
            &workflow_status,
            &workflow_status_hash,
        )
        .unwrap();
        assert!(workflow_status_identifier
            .contains("workflow_status-linear-sess:current-phase-0:completed-0:complete-false"));

        let verify_chain = base_response(serde_json::json!({
            "valid": true,
            "phases_verified": 1,
            "steps_verified": 2,
            "errors": []
        }));
        let verify_chain_hash = sha256_json(&verify_chain);
        let verify_chain_identifier = mcp_proof_read_identifier(
            McpProofReadSurface::VerifyChain,
            &verify_chain,
            &verify_chain_hash,
        )
        .unwrap();
        assert!(verify_chain_identifier
            .contains("verify_chain-linear-sess:valid-true:phases-1:steps-2:errors-0"));
    }

    #[test]
    fn proof_read_identifier_rejects_missing_step_identity() {
        let response = base_response(serde_json::json!({
            "phase_id": "claim",
            "combined_hash": HASH
        }));
        let hash = sha256_json(&response);
        let err = mcp_proof_read_identifier(McpProofReadSurface::StepProof, &response, &hash)
            .expect_err("step proof identifier must require concrete step_id");
        assert!(err.contains("step_id"), "{err}");
    }

    #[tokio::test]
    async fn graph_authorizes_mcp_proof_read_surfaces() {
        let graph = build_mcp_proof_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let cases = [
            (
                McpProofReadSurface::ProofChain,
                base_response(serde_json::json!({"entries": []})),
            ),
            (
                McpProofReadSurface::StepProof,
                base_response(serde_json::json!({
                    "step_id": "1",
                    "phase_id": "claim",
                    "combined_hash": HASH
                })),
            ),
            (
                McpProofReadSurface::WorkflowStatus,
                base_response(serde_json::json!({
                    "current_phase": 0,
                    "completed_phases": [],
                    "complete": false
                })),
            ),
            (
                McpProofReadSurface::VerifyChain,
                base_response(serde_json::json!({
                    "valid": true,
                    "phases_verified": 1,
                    "steps_verified": 2,
                    "errors": []
                })),
            ),
            (
                McpProofReadSurface::StepChain,
                base_response(serde_json::json!({
                    "steps": [],
                    "head_hash": HASH
                })),
            ),
            (
                McpProofReadSurface::ActiveStep,
                base_response(serde_json::json!({
                    "last_step": null,
                    "chain_length": 0,
                    "head_hash": HASH
                })),
            ),
        ];

        for (surface, response) in cases {
            let hash = sha256_json(&response);
            let state = McpProofReadState::from_response(
                surface,
                mcp_proof_read_identifier(surface, &response, &hash).expect("proof identifier"),
                &response,
            );
            let run = run_mcp_proof_read_decision_report(&graph, state)
                .await
                .unwrap();
            assert_eq!(run.state.decision, McpProofReadDecision::Verified);
            assert!(!run.state.workflow_authority_present);
            assert!(run
                .mcp_proof_read_authorization()
                .unwrap()
                .unwrap()
                .checkpoint_ref()
                .contains('#'));
        }
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let graph = build_mcp_proof_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = base_response(serde_json::json!({
            "entries": [],
            "combined_hash": HASH,
            "head_hash": HASH,
            "chain_length": 0
        }));
        let hash = sha256_json(&response);
        let state = McpProofReadState::from_response(
            McpProofReadSurface::ProofChain,
            mcp_proof_read_identifier(McpProofReadSurface::ProofChain, &response, &hash)
                .expect("proof identifier"),
            &response,
        );
        let mut run = run_mcp_proof_read_decision_report(&graph, state)
            .await
            .unwrap();
        run.write_history.clear();

        let err = run
            .mcp_proof_read_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_rejects_mcp_proof_read_without_workflow_projection() {
        let graph = build_mcp_proof_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = serde_json::json!({
            "skill": "linear",
            "session_id": "sess",
            "entries": []
        });
        let state = McpProofReadState::from_response(
            McpProofReadSurface::ProofChain,
            "missing-workflow",
            &response,
        );
        let err = run_mcp_proof_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("LangGraph workflow projection"));
    }

    #[tokio::test]
    async fn graph_rejects_verify_chain_without_verification_fields() {
        let graph = build_mcp_proof_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = base_response(serde_json::json!({
            "valid": true,
            "phases_verified": 1,
            "steps_verified": 2
        }));
        let state = McpProofReadState::from_response(
            McpProofReadSurface::VerifyChain,
            "missing-verification-errors",
            &response,
        );
        let err = run_mcp_proof_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("verification result fields"), "{err}");
    }

    #[tokio::test]
    async fn graph_rejects_explicit_workflow_authority_before_read_audit() {
        let graph = build_mcp_proof_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut response = base_response(serde_json::json!({"entries": []}));
        response.as_object_mut().unwrap().insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
        let state = McpProofReadState::from_response(
            McpProofReadSurface::ProofChain,
            "forged-authority",
            &response,
        );
        let err = run_mcp_proof_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(
            err.contains("before read graph audit"),
            "unexpected error: {err}"
        );
    }
}
