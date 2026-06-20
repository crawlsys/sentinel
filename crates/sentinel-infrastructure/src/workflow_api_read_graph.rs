//! Graph-backed audit for local API workflow read responses.
//!
//! Workflow API responses expose phase-graph topology, checkpoint history, and
//! write history. This graph checkpoints the rendered API response itself so
//! the `workflow_authority = "langgraph"` claim is backed by durable
//! LangGraph evidence at the API boundary.

use std::{io::Write as _, time::Duration};

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
pub enum WorkflowApiReadSurface {
    List,
    Item,
    Status,
    PhaseSteps,
    Progress,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WorkflowApiReadDecision {
    #[default]
    Unclassified,
    Verified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowApiReadState {
    pub identifier: String,
    pub surface: WorkflowApiReadSurface,
    pub response_sha256: String,
    pub response_len: u64,
    pub workflow_authority_langgraph: bool,
    pub workflows_count: Option<u64>,
    pub child_authority_count: u64,
    pub child_graph_audit_count: u64,
    pub skill_present: bool,
    pub error_present: bool,
    pub status_no_checkpoint: bool,
    pub phase_id_present: bool,
    pub steps_count: Option<u64>,
    pub phases_count: Option<u64>,
    pub overall_present: bool,
    pub topology_present: bool,
    pub checkpoints_present: bool,
    pub history_present: bool,
    pub writes_present: bool,
    pub latest_checkpoint_present: bool,
    pub graph_state_present: bool,
    pub decision: WorkflowApiReadDecision,
}

impl WorkflowApiReadState {
    #[must_use]
    pub fn from_response(
        surface: WorkflowApiReadSurface,
        identifier: impl Into<String>,
        response: &Value,
    ) -> Self {
        let response_bytes =
            serde_json::to_vec(response).expect("workflow API read response must serialize");
        let workflows = response.get("workflows").and_then(Value::as_array);
        let steps = response.get("steps").and_then(Value::as_array);
        let phases = response.get("phases").and_then(Value::as_array);
        Self {
            identifier: identifier.into(),
            surface,
            response_sha256: hex::encode(Sha256::digest(&response_bytes)),
            response_len: response_bytes.len() as u64,
            workflow_authority_langgraph: response
                .get("workflow_authority")
                .and_then(Value::as_str)
                == Some("langgraph"),
            workflows_count: workflows.map(|items| items.len() as u64),
            child_authority_count: workflows.map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| {
                        item.get("workflow_authority").and_then(Value::as_str) == Some("langgraph")
                    })
                    .count() as u64
            }),
            child_graph_audit_count: workflows.map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| item.get("graph_audit").is_some())
                    .count() as u64
            }),
            skill_present: response
                .get("skill")
                .and_then(Value::as_str)
                .is_some_and(|skill| !skill.trim().is_empty()),
            error_present: response
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.trim().is_empty()),
            status_no_checkpoint: response.get("status").and_then(Value::as_str)
                == Some("no_checkpoint"),
            phase_id_present: response
                .get("phase_id")
                .and_then(Value::as_str)
                .is_some_and(|phase| !phase.trim().is_empty()),
            steps_count: steps.map(|items| items.len() as u64),
            phases_count: phases.map(|items| items.len() as u64),
            overall_present: response.get("overall").and_then(Value::as_object).is_some(),
            topology_present: response.get("graph_topology").is_some(),
            checkpoints_present: response
                .get("graph_checkpoints")
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            history_present: response
                .get("graph_history")
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            writes_present: response
                .get("graph_writes")
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty()),
            latest_checkpoint_present: response.get("latest_checkpoint").is_some(),
            graph_state_present: response
                .get("graph_state")
                .is_some_and(|state| !state.is_null()),
            decision: WorkflowApiReadDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkflowApiReadGraphRun {
    pub state: WorkflowApiReadState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<WorkflowApiReadState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct WorkflowApiReadAuthorization {
    decision: WorkflowApiReadDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl WorkflowApiReadAuthorization {
    #[must_use]
    pub fn decision(&self) -> WorkflowApiReadDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl WorkflowApiReadGraphRun {
    #[must_use]
    pub fn workflow_api_read_authorization(
        &self,
    ) -> Result<Option<WorkflowApiReadAuthorization>, String> {
        if self.state.decision == WorkflowApiReadDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "workflow_api_read",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(WorkflowApiReadAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFIED: &str = "verified";

pub type WorkflowApiReadGraph = CompilationResult<WorkflowApiReadState>;

#[must_use]
pub fn workflow_api_read_decision_label(decision: WorkflowApiReadDecision) -> &'static str {
    match decision {
        WorkflowApiReadDecision::Unclassified => "unclassified",
        WorkflowApiReadDecision::Verified => "verified",
    }
}

#[must_use]
pub fn workflow_api_read_surface_label(surface: WorkflowApiReadSurface) -> &'static str {
    match surface {
        WorkflowApiReadSurface::List => "list",
        WorkflowApiReadSurface::Item => "item",
        WorkflowApiReadSurface::Status => "status",
        WorkflowApiReadSurface::PhaseSteps => "phase_steps",
        WorkflowApiReadSurface::Progress => "progress",
        WorkflowApiReadSurface::Error => "error",
    }
}

#[must_use]
pub fn sha256_json(value: &Value) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(value).expect("workflow API read JSON value must serialize"),
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
        .with_metadata("sentinel.graph", "workflow_api_read")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn workflow_api_read_state_schema() -> StateSchema<WorkflowApiReadState> {
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
                "workflow_authority_langgraph",
                "workflows_count",
                "child_authority_count",
                "child_graph_audit_count",
                "skill_present",
                "error_present",
                "status_no_checkpoint",
                "phase_id_present",
                "steps_count",
                "phases_count",
                "overall_present",
                "topology_present",
                "checkpoints_present",
                "history_present",
                "writes_present",
                "latest_checkpoint_present",
                "graph_state_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "surface": {
                    "type": "string",
                    "enum": ["List", "Item", "Status", "PhaseSteps", "Progress", "Error"]
                },
                "response_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "response_len": { "type": "integer", "minimum": 1 },
                "workflow_authority_langgraph": { "type": "boolean" },
                "workflows_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "child_authority_count": { "type": "integer", "minimum": 0 },
                "child_graph_audit_count": { "type": "integer", "minimum": 0 },
                "skill_present": { "type": "boolean" },
                "error_present": { "type": "boolean" },
                "status_no_checkpoint": { "type": "boolean" },
                "phase_id_present": { "type": "boolean" },
                "steps_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "phases_count": {
                    "anyOf": [
                        { "type": "integer", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "overall_present": { "type": "boolean" },
                "topology_present": { "type": "boolean" },
                "checkpoints_present": { "type": "boolean" },
                "history_present": { "type": "boolean" },
                "writes_present": { "type": "boolean" },
                "latest_checkpoint_present": { "type": "boolean" },
                "graph_state_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Verified"]
                }
            },
            "x-sentinel": {
                "graph": "workflow_api_read",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &WorkflowApiReadState| {
            if state.response_len == 0 || !hex_digest_present(&state.response_sha256) {
                return Err(StateError::ValidationFailed(
                    "workflow API response digest must identify a serialized response".to_string(),
                ));
            }
            if !state.workflow_authority_langgraph && !state.error_present {
                return Err(StateError::ValidationFailed(
                    "non-error workflow API response must declare LangGraph workflow authority"
                        .to_string(),
                ));
            }
            match state.surface {
                WorkflowApiReadSurface::List => {
                    let Some(count) = state.workflows_count else {
                        return Err(StateError::ValidationFailed(
                            "workflow API list response requires workflows array".to_string(),
                        ));
                    };
                    if state.child_authority_count != count {
                        return Err(StateError::ValidationFailed(
                            "workflow API list response requires all workflow items to declare LangGraph authority"
                                .to_string(),
                        ));
                    }
                    if state.child_graph_audit_count != count {
                        return Err(StateError::ValidationFailed(
                            "workflow API list response requires all workflow items to carry graph audit"
                                .to_string(),
                        ));
                    }
                }
                WorkflowApiReadSurface::Item => {
                    if !state.skill_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API item response requires skill or error".to_string(),
                        ));
                    }
                    if !state.error_present && !state.topology_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API item response requires phase graph topology".to_string(),
                        ));
                    }
                }
                WorkflowApiReadSurface::Status => {
                    if !state.skill_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API status response requires skill or error".to_string(),
                        ));
                    }
                    if !state.error_present && !state.topology_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API status response requires phase graph topology".to_string(),
                        ));
                    }
                }
                WorkflowApiReadSurface::PhaseSteps => {
                    if !state.skill_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API phase steps response requires skill or error".to_string(),
                        ));
                    }
                    if !state.error_present && !state.phase_id_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API phase steps response requires phase_id".to_string(),
                        ));
                    }
                    if !state.error_present && state.steps_count.is_none() {
                        return Err(StateError::ValidationFailed(
                            "workflow API phase steps response requires steps array".to_string(),
                        ));
                    }
                    if !state.error_present && !state.topology_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API phase steps response requires phase graph topology"
                                .to_string(),
                        ));
                    }
                }
                WorkflowApiReadSurface::Progress => {
                    if !state.skill_present && !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API progress response requires skill or error".to_string(),
                        ));
                    }
                    if !state.error_present && state.phases_count.is_none() {
                        return Err(StateError::ValidationFailed(
                            "workflow API progress response requires phases array".to_string(),
                        ));
                    }
                    if !state.error_present && !state.overall_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API progress response requires overall summary".to_string(),
                        ));
                    }
                    if !state.error_present && !state.topology_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API progress response requires phase graph topology"
                                .to_string(),
                        ));
                    }
                }
                WorkflowApiReadSurface::Error => {
                    if !state.error_present {
                        return Err(StateError::ValidationFailed(
                            "workflow API error response requires error text".to_string(),
                        ));
                    }
                }
            }
            Ok(())
        })
}

async fn classify_node(state: WorkflowApiReadState) -> Result<WorkflowApiReadState, NodeError> {
    let mut next = state;
    next.decision = WorkflowApiReadDecision::Verified;
    Ok(next)
}

async fn terminal_node(state: WorkflowApiReadState) -> Result<WorkflowApiReadState, NodeError> {
    let mut next = state;
    next.decision = WorkflowApiReadDecision::Verified;
    Ok(next)
}

pub async fn build_workflow_api_read_graph() -> Result<WorkflowApiReadGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("workflow_api_read").await?;
    build_workflow_api_read_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_workflow_api_read_graph_with_ephemeral_sqlite(
) -> Result<WorkflowApiReadGraph, String> {
    build_workflow_api_read_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_workflow_api_read_graph_with_database_path(
    db_path: &str,
) -> Result<WorkflowApiReadGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_workflow_api_read_graph_with_checkpointer(checkpointer).await
}

async fn build_workflow_api_read_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<WorkflowApiReadGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = workflow_api_read_state_schema();
    let builder = StateGraphBuilder::<WorkflowApiReadState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: WorkflowApiReadState| async move {
                emit_decision_node_event("workflow_api_read", CLASSIFY, &s.identifier)?;
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
            |s: WorkflowApiReadState| async move {
                emit_decision_node_event("workflow_api_read", VERIFIED, &s.identifier)?;
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
        .add_conditional_edge(CLASSIFY, |_s: &WorkflowApiReadState| VERIFIED.into())
        .add_edge(VERIFIED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_workflow_api_read_decision_report(
    compiled: &WorkflowApiReadGraph,
    state: WorkflowApiReadState,
) -> Result<WorkflowApiReadGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "workflow_api_read",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "workflow_api_read",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "workflow_api_read",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(WorkflowApiReadGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: workflow_api_read_graph_topology(compiled)?,
    })
}

pub fn workflow_api_read_graph_topology(
    compiled: &WorkflowApiReadGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("workflow_api_read", compiled)
}

pub async fn workflow_api_read_graph_audit(
    surface: WorkflowApiReadSurface,
    response: &Value,
) -> Result<Value, String> {
    let graph = build_workflow_api_read_graph()
        .await
        .map_err(|e| format!("build workflow API read graph: {e}"))?;
    workflow_api_read_graph_audit_with_graph(&graph, surface, response).await
}

pub async fn workflow_api_read_graph_audit_with_graph(
    graph: &WorkflowApiReadGraph,
    surface: WorkflowApiReadSurface,
    response: &Value,
) -> Result<Value, String> {
    let response_hash = sha256_json(response);
    let surface_label = workflow_api_read_surface_label(surface);
    let identifier = workflow_api_read_identifier(surface_label, response, &response_hash);
    let state = WorkflowApiReadState::from_response(surface, identifier, response);
    let run = run_workflow_api_read_decision_report(graph, state)
        .await
        .map_err(|e| format!("run workflow API read graph: {e}"))?;
    let authorization = run
        .workflow_api_read_authorization()
        .map_err(|e| format!("workflow API read graph authorization failed: {e}"))?
        .ok_or_else(|| "workflow API read graph produced no terminal checkpoint".to_string())?;
    let graph_runs = crate::paths::sentinel_root()
        .join("metrics")
        .join("workflow-api-read.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "create workflow API read graph audit dir {}: {e}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "workflow_api_read",
        "surface": surface_label,
        "response_sha256": response_hash,
        "decision": workflow_api_read_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .map_err(|e| {
            format!(
                "open workflow API read graph audit {}: {e}",
                graph_runs.display()
            )
        })?;
    serde_json::to_writer(&mut file, &row).map_err(|e| {
        format!(
            "write workflow API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").map_err(|e| {
        format!(
            "terminate workflow API read graph audit {}: {e}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "workflow_api_read",
        "surface": surface_label,
        "graph_runs_path": graph_runs,
        "response_sha256": row["response_sha256"].clone(),
        "decision": row["decision"].clone(),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

#[must_use]
pub fn workflow_api_read_identifier(
    surface_label: &str,
    response: &Value,
    response_hash: &str,
) -> String {
    let id = if let Some(skill) = response.get("skill").and_then(Value::as_str) {
        format!("skill-present-true:{skill}")
    } else if let Some(session_id) = response.get("session_id").and_then(Value::as_str) {
        format!("skill-present-false:session-id-present-true:{session_id}")
    } else {
        "skill-present-false:session-id-present-false".to_string()
    };
    format!("{surface_label}-{id}:response-{response_hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topology() -> Value {
        serde_json::json!({
            "graph": "phase",
            "durable_checkpointer": true
        })
    }

    fn item_response() -> Value {
        serde_json::json!({
            "skill": "linear",
            "workflow_authority": "langgraph",
            "status": "no_checkpoint",
            "graph_topology": topology(),
            "graph_checkpoints": [],
            "graph_history": [],
            "graph_writes": []
        })
    }

    fn audited_item_response() -> Value {
        let mut item = item_response();
        item.as_object_mut().unwrap().insert(
            "graph_audit".to_string(),
            serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "workflow_api_read"
            }),
        );
        item
    }

    fn phase_steps_response() -> Value {
        serde_json::json!({
            "workflow_authority": "langgraph",
            "skill": "linear",
            "phase_id": "claim",
            "steps": [{
                "id": "0.1",
                "description": "Fetch ticket",
                "status": "pending"
            }],
            "completed": 0,
            "total": 1,
            "graph_topology": topology(),
            "graph_checkpoints": [],
            "graph_history": [],
            "graph_writes": []
        })
    }

    fn progress_response() -> Value {
        serde_json::json!({
            "workflow_authority": "langgraph",
            "skill": "linear",
            "phases": [{
                "id": "claim",
                "description": "Claim",
                "status": "pending",
                "steps_completed": 0,
                "steps_total": 1
            }],
            "overall": {
                "steps_completed": 0,
                "steps_total": 1,
                "percentage": 0
            },
            "graph_topology": topology(),
            "graph_checkpoints": [],
            "graph_history": [],
            "graph_writes": []
        })
    }

    #[tokio::test]
    async fn graph_authorizes_workflow_item_response() {
        let graph = build_workflow_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = WorkflowApiReadState::from_response(
            WorkflowApiReadSurface::Item,
            "item-linear",
            &item_response(),
        );
        let run = run_workflow_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, WorkflowApiReadDecision::Verified);
        assert!(run
            .workflow_api_read_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_phase_steps_response() {
        let graph = build_workflow_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = WorkflowApiReadState::from_response(
            WorkflowApiReadSurface::PhaseSteps,
            "phase-steps-linear-claim",
            &phase_steps_response(),
        );
        let run = run_workflow_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, WorkflowApiReadDecision::Verified);
        assert_eq!(run.state.steps_count, Some(1));
        assert!(run
            .workflow_api_read_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_progress_response() {
        let graph = build_workflow_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = WorkflowApiReadState::from_response(
            WorkflowApiReadSurface::Progress,
            "progress-linear",
            &progress_response(),
        );
        let run = run_workflow_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, WorkflowApiReadDecision::Verified);
        assert_eq!(run.state.phases_count, Some(1));
        assert!(run.state.overall_present);
        assert!(run
            .workflow_api_read_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_rejects_phase_steps_without_steps_array() {
        let graph = build_workflow_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut response = phase_steps_response();
        response.as_object_mut().unwrap().remove("steps");
        let state = WorkflowApiReadState::from_response(
            WorkflowApiReadSurface::PhaseSteps,
            "phase-steps-missing-steps",
            &response,
        );
        let err = run_workflow_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("steps array"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn graph_authorizes_workflow_list_response_with_audited_children() {
        let graph = build_workflow_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = serde_json::json!({
            "workflow_authority": "langgraph",
            "workflows": [audited_item_response()]
        });
        let state =
            WorkflowApiReadState::from_response(WorkflowApiReadSurface::List, "list", &response);
        let run = run_workflow_api_read_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, WorkflowApiReadDecision::Verified);
        assert_eq!(run.state.child_authority_count, 1);
        assert_eq!(run.state.child_graph_audit_count, 1);
    }

    #[tokio::test]
    async fn graph_rejects_workflow_list_with_unaudited_children() {
        let graph = build_workflow_api_read_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let response = serde_json::json!({
            "workflow_authority": "langgraph",
            "workflows": [item_response()]
        });
        let state = WorkflowApiReadState::from_response(
            WorkflowApiReadSurface::List,
            "list-forged",
            &response,
        );
        let err = run_workflow_api_read_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("graph audit"), "unexpected error: {err}");
    }
}
