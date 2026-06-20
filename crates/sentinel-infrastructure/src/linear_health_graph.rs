//! Graph-backed Linear board health verdict.
//!
//! The health scanner computes numeric board metrics. This graph is the
//! checkpointed LangGraph authority that validates those metrics and emits the
//! board-health decision exposed by CLI and MCP.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::linear_health_score::{HealthSummary, GRADE_HEALTHY_MIN, GRADE_OK_MIN};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LinearHealthDecision {
    #[default]
    Unclassified,
    Healthy,
    Ok,
    NeedsWork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearHealthState {
    pub identifier: String,
    pub issues_total: usize,
    pub total_score: u32,
    pub hygiene_score: f64,
    pub structure_score: f64,
    pub data_quality_score: f64,
    pub flow_score: f64,
    pub qa_congestion_fraction: f64,
    pub qa_failed_count: usize,
    pub grade: String,
    pub decision: LinearHealthDecision,
}

impl LinearHealthState {
    #[must_use]
    pub fn from_summary(summary: &HealthSummary) -> Self {
        Self {
            identifier: "board".to_string(),
            issues_total: summary.issues_total,
            total_score: summary.total_score,
            hygiene_score: summary.hygiene_score,
            structure_score: summary.structure_score,
            data_quality_score: summary.data_quality_score,
            flow_score: summary.flow_score,
            qa_congestion_fraction: summary.qa_congestion_fraction,
            qa_failed_count: summary.qa_failed_count,
            grade: summary.grade.clone(),
            decision: LinearHealthDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LinearHealthGraphRun {
    pub state: LinearHealthState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<LinearHealthState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct LinearHealthAuthorization {
    decision: LinearHealthDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl LinearHealthAuthorization {
    #[must_use]
    pub fn decision(&self) -> LinearHealthDecision {
        self.decision
    }

    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl LinearHealthGraphRun {
    #[must_use]
    pub fn health_authorization(&self) -> Result<Option<LinearHealthAuthorization>, String> {
        if self.state.decision == LinearHealthDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "linear_health",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(LinearHealthAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const HEALTHY: &str = "healthy";
const OK: &str = "ok";
const NEEDS_WORK: &str = "needs_work";

pub type LinearHealthGraph = CompilationResult<LinearHealthState>;

#[must_use]
pub fn linear_health_decision_label(decision: LinearHealthDecision) -> &'static str {
    match decision {
        LinearHealthDecision::Unclassified => "unclassified",
        LinearHealthDecision::Healthy => "healthy",
        LinearHealthDecision::Ok => "ok",
        LinearHealthDecision::NeedsWork => "needs-work",
    }
}

fn expected_decision(total_score: u32) -> LinearHealthDecision {
    if f64::from(total_score) >= GRADE_HEALTHY_MIN {
        LinearHealthDecision::Healthy
    } else if f64::from(total_score) >= GRADE_OK_MIN {
        LinearHealthDecision::Ok
    } else {
        LinearHealthDecision::NeedsWork
    }
}

fn expected_grade(total_score: u32) -> &'static str {
    linear_health_decision_label(expected_decision(total_score))
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "linear_health")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn linear_health_state_schema() -> StateSchema<LinearHealthState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "issues_total",
                "identifier",
                "total_score",
                "hygiene_score",
                "structure_score",
                "data_quality_score",
                "flow_score",
                "qa_congestion_fraction",
                "qa_failed_count",
                "grade",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "issues_total": { "type": "integer", "minimum": 0 },
                "total_score": { "type": "integer", "minimum": 0, "maximum": 100 },
                "hygiene_score": { "type": "number", "minimum": 0, "maximum": 30 },
                "structure_score": { "type": "number", "minimum": 0, "maximum": 20 },
                "data_quality_score": { "type": "number", "minimum": 0, "maximum": 15 },
                "flow_score": { "type": "number", "minimum": 0, "maximum": 35 },
                "qa_congestion_fraction": { "type": "number", "minimum": 0, "maximum": 1 },
                "qa_failed_count": { "type": "integer", "minimum": 0 },
                "grade": { "type": "string", "enum": ["healthy", "ok", "needs-work"] },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Healthy", "Ok", "NeedsWork"]
                }
            },
            "x-sentinel": {
                "graph": "linear_health",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &LinearHealthState| {
            let finite_scores = [
                state.hygiene_score,
                state.structure_score,
                state.data_quality_score,
                state.flow_score,
                state.qa_congestion_fraction,
            ];
            if finite_scores.iter().any(|score| !score.is_finite()) {
                return Err(StateError::ValidationFailed(
                    "linear_health scores must be finite".to_string(),
                ));
            }
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "linear_health identifier must not be empty".to_string(),
                ));
            }
            if state.total_score > 100 {
                return Err(StateError::ValidationFailed(
                    "linear_health total_score must be <= 100".to_string(),
                ));
            }
            let expected_grade = expected_grade(state.total_score);
            if state.grade != expected_grade {
                return Err(StateError::ValidationFailed(format!(
                    "linear_health grade '{}' does not match score {} ({expected_grade})",
                    state.grade, state.total_score
                )));
            }
            if state.decision != LinearHealthDecision::Unclassified
                && state.decision != expected_decision(state.total_score)
            {
                return Err(StateError::ValidationFailed(
                    "linear_health terminal decision must match the score band".to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_linear_health_graph() -> Result<LinearHealthGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("linear_health").await?;
    build_linear_health_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_linear_health_graph_with_ephemeral_sqlite() -> Result<LinearHealthGraph, String> {
    build_linear_health_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_linear_health_graph_with_database_path(
    database_path: &str,
) -> Result<LinearHealthGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_linear_health_graph_with_checkpointer(checkpointer).await
}

async fn build_linear_health_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<LinearHealthGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = linear_health_state_schema();
    let builder = StateGraphBuilder::<LinearHealthState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: LinearHealthState| async move {
                emit_decision_node_event("linear_health", CLASSIFY, "board")?;
                Ok::<_, NodeError>(s)
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
            HEALTHY,
            |s: LinearHealthState| async move {
                emit_decision_node_event("linear_health", HEALTHY, "board")?;
                let mut next = s;
                next.decision = LinearHealthDecision::Healthy;
                Ok::<_, NodeError>(next)
            },
            node_config(
                HEALTHY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            OK,
            |s: LinearHealthState| async move {
                emit_decision_node_event("linear_health", OK, "board")?;
                let mut next = s;
                next.decision = LinearHealthDecision::Ok;
                Ok::<_, NodeError>(next)
            },
            node_config(
                OK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            NEEDS_WORK,
            |s: LinearHealthState| async move {
                emit_decision_node_event("linear_health", NEEDS_WORK, "board")?;
                let mut next = s;
                next.decision = LinearHealthDecision::NeedsWork;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NEEDS_WORK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &LinearHealthState| {
            match expected_decision(s.total_score) {
                LinearHealthDecision::Healthy => HEALTHY.into(),
                LinearHealthDecision::Ok => OK.into(),
                LinearHealthDecision::NeedsWork => NEEDS_WORK.into(),
                LinearHealthDecision::Unclassified => NEEDS_WORK.into(),
            }
        })
        .add_edge(HEALTHY, END)
        .add_edge(OK, END)
        .add_edge(NEEDS_WORK, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_linear_health_decision_report(
    compiled: &LinearHealthGraph,
    state: LinearHealthState,
) -> Result<LinearHealthGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "linear_health",
        "board",
        &state,
    )?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "linear_health", "board", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "linear_health",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(LinearHealthGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: linear_health_graph_topology(compiled)?,
    })
}

pub fn linear_health_graph_topology(
    compiled: &LinearHealthGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("linear_health", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(total_score: u32, grade: &str) -> HealthSummary {
        HealthSummary {
            issues_total: 4,
            total_score,
            hygiene_score: 20.0,
            structure_score: 20.0,
            data_quality_score: 15.0,
            flow_score: f64::from(total_score).saturating_sub(55.0),
            qa_congestion_fraction: 0.0,
            qa_failed_count: 0,
            grade: grade.to_string(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_healthy_board() {
        let graph = build_linear_health_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = LinearHealthState::from_summary(&summary(90, "healthy"));
        let run = run_linear_health_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, LinearHealthDecision::Healthy);
        assert_eq!(run.topology.graph, "linear_health");
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert!(
            run.stream
                .iter()
                .any(|part| part.event_type == "ExecutionComplete"),
            "stream must expose LangGraph execution completion"
        );
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state write history"
        );
        assert!(
            run.write_history
                .iter()
                .filter(|write| write.channel == "state")
                .any(|write| write.value_json["decision"] == "Healthy"),
            "state write history must decode the terminal decision JSON"
        );
        let authorization = run
            .health_authorization()
            .expect("health decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), LinearHealthDecision::Healthy);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_needs_work_board() {
        let graph = build_linear_health_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = LinearHealthState::from_summary(&summary(42, "needs-work"));
        let run = run_linear_health_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, LinearHealthDecision::NeedsWork);
        assert_eq!(
            run.health_authorization()
                .expect("health decision should have checkpoint authorization")
                .expect("authorization")
                .decision(),
            LinearHealthDecision::NeedsWork
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_grade() {
        let graph = build_linear_health_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = LinearHealthState::from_summary(&summary(42, "healthy"));
        let err = run_linear_health_decision_report(&graph, state)
            .await
            .expect_err("forged grade should fail schema validation");
        assert!(err.contains("does not match score"));
    }

    trait SaturatingSubF64 {
        fn saturating_sub(self, rhs: f64) -> f64;
    }

    impl SaturatingSubF64 for f64 {
        fn saturating_sub(self, rhs: f64) -> f64 {
            (self - rhs).max(0.0)
        }
    }
}
