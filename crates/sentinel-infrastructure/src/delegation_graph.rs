//! Graph-backed worker delegation authorization.
//!
//! Delegation tools call an external worker model and return the worker's
//! output to the orchestrator. This graph does not replace that LLM call; it
//! validates and checkpoints the request/result pair so MCP delegation returns
//! durable LangGraph evidence instead of unaudited text.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::delegation_service::{DelegationRequest, DelegationResult};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DelegationDecision {
    #[default]
    Unclassified,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationState {
    pub identifier: String,
    pub worker: String,
    pub task_sha256: String,
    pub task_len: u64,
    pub context_sha256: String,
    pub context_len: u64,
    pub max_tokens: u64,
    pub result_worker: String,
    pub worker_matches: bool,
    pub output_sha256: String,
    pub output_len: u64,
    pub output_present: bool,
    pub decision: DelegationDecision,
}

impl DelegationState {
    #[must_use]
    pub fn from_result(
        identifier: impl Into<String>,
        request: &DelegationRequest,
        result: &DelegationResult,
    ) -> Self {
        let worker = request.worker.label().to_string();
        Self {
            identifier: identifier.into(),
            worker: worker.clone(),
            task_sha256: sha256(&request.task),
            task_len: request.task.len() as u64,
            context_sha256: sha256(&request.context),
            context_len: request.context.len() as u64,
            max_tokens: u64::from(request.max_tokens),
            result_worker: result.worker.clone(),
            worker_matches: result.worker == worker,
            output_sha256: sha256(&result.output),
            output_len: result.output.len() as u64,
            output_present: !result.output.trim().is_empty(),
            decision: DelegationDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DelegationGraphRun {
    pub state: DelegationState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DelegationState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct DelegationAuthorization {
    decision: DelegationDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl DelegationAuthorization {
    #[must_use]
    pub fn decision(&self) -> DelegationDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl DelegationGraphRun {
    pub fn delegation_authorization(&self) -> Result<Option<DelegationAuthorization>, String> {
        if self.state.decision == DelegationDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "delegation",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(DelegationAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const COMPLETED: &str = "completed";

pub type DelegationGraph = CompilationResult<DelegationState>;

#[must_use]
pub fn delegation_decision_label(decision: DelegationDecision) -> &'static str {
    match decision {
        DelegationDecision::Unclassified => "unclassified",
        DelegationDecision::Completed => "completed",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
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
        .with_metadata("sentinel.graph", "delegation")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn delegation_state_schema() -> StateSchema<DelegationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "worker",
                "task_sha256",
                "task_len",
                "context_sha256",
                "context_len",
                "max_tokens",
                "result_worker",
                "worker_matches",
                "output_sha256",
                "output_len",
                "output_present",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "worker": { "type": "string", "enum": ["codex", "kimi"] },
                "task_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "task_len": { "type": "integer", "minimum": 1 },
                "context_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "context_len": { "type": "integer", "minimum": 0 },
                "max_tokens": { "type": "integer", "minimum": 1 },
                "result_worker": { "type": "string", "minLength": 1 },
                "worker_matches": { "type": "boolean" },
                "output_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "output_len": { "type": "integer", "minimum": 1 },
                "output_present": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Completed"]
                }
            },
            "x-sentinel": {
                "graph": "delegation",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &DelegationState| {
            if state.task_len == 0 {
                return Err(StateError::ValidationFailed(
                    "delegation task must be non-empty".to_string(),
                ));
            }
            if state.max_tokens == 0 {
                return Err(StateError::ValidationFailed(
                    "delegation max_tokens must be non-zero".to_string(),
                ));
            }
            for (name, digest) in [
                ("task_sha256", &state.task_sha256),
                ("context_sha256", &state.context_sha256),
                ("output_sha256", &state.output_sha256),
            ] {
                if !hex_digest_present(digest) {
                    return Err(StateError::ValidationFailed(format!(
                        "delegation {name} must be a 64-character hex digest"
                    )));
                }
            }
            if !matches!(state.worker.as_str(), "codex" | "kimi") {
                return Err(StateError::ValidationFailed(
                    "delegation worker must be codex or kimi".to_string(),
                ));
            }
            if !state.worker_matches || state.result_worker != state.worker {
                return Err(StateError::ValidationFailed(
                    "delegation result worker must match requested worker".to_string(),
                ));
            }
            if state.output_len == 0 || !state.output_present {
                return Err(StateError::ValidationFailed(
                    "delegation completed result requires non-empty output".to_string(),
                ));
            }
            if state.decision != DelegationDecision::Unclassified
                && state.decision != DelegationDecision::Completed
            {
                return Err(StateError::ValidationFailed(
                    "delegation terminal decision must be completed".to_string(),
                ));
            }
            Ok(())
        })
}

async fn classify_node(state: DelegationState) -> Result<DelegationState, NodeError> {
    let mut next = state;
    next.decision = DelegationDecision::Completed;
    Ok(next)
}

async fn terminal_node(state: DelegationState) -> Result<DelegationState, NodeError> {
    let mut next = state;
    next.decision = DelegationDecision::Completed;
    Ok(next)
}

pub async fn build_delegation_graph() -> Result<DelegationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("delegation").await?;
    build_delegation_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_delegation_graph_with_ephemeral_sqlite() -> Result<DelegationGraph, String> {
    build_delegation_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_delegation_graph_with_database_path(
    db_path: &str,
) -> Result<DelegationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_delegation_graph_with_checkpointer(checkpointer).await
}

async fn build_delegation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<DelegationGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = delegation_state_schema();
    let builder = StateGraphBuilder::<DelegationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: DelegationState| async move {
                emit_decision_node_event("delegation", CLASSIFY, &s.identifier)?;
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
            |s: DelegationState| async move {
                emit_decision_node_event("delegation", COMPLETED, &s.identifier)?;
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
        .add_conditional_edge(CLASSIFY, |_s: &DelegationState| COMPLETED.into())
        .add_edge(COMPLETED, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_delegation_decision_report(
    compiled: &DelegationGraph,
    state: DelegationState,
) -> Result<DelegationGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "delegation",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "delegation", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "delegation",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(DelegationGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: delegation_graph_topology(compiled)?,
    })
}

pub fn delegation_graph_topology(
    compiled: &DelegationGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("delegation", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::delegation_service::{DelegationRequest, DelegationResult, Worker};

    fn request() -> DelegationRequest {
        DelegationRequest {
            worker: Worker::Codex,
            task: "review this change".to_string(),
            context: "diff --git".to_string(),
            max_tokens: 512,
        }
    }

    fn result() -> DelegationResult {
        DelegationResult {
            worker: "codex".to_string(),
            output: "the change is sound".to_string(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_delegation_result() {
        let graph = build_delegation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = DelegationState::from_result("delegation-test", &request(), &result());
        let run = run_delegation_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, DelegationDecision::Completed);
        assert!(run
            .delegation_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let graph = build_delegation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = DelegationState::from_result("delegation-test", &request(), &result());
        let mut run = run_delegation_decision_report(&graph, state).await.unwrap();
        run.write_history.clear();

        let err = run
            .delegation_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_worker_mismatch() {
        let graph = build_delegation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut forged = result();
        forged.worker = "kimi".to_string();
        let state = DelegationState::from_result("delegation-forged", &request(), &forged);
        let err = run_delegation_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("worker"), "{err}");
    }
}
