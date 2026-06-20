//! Graph-backed developer scorecard classification.
//!
//! The scorecard scanner computes per-developer delivery metrics. This graph is
//! the checkpointed LangGraph authority that classifies each row before CLI and
//! MCP expose it as an operational verdict.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::dev_scorecard::DevScore;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DevScorecardDecision {
    #[default]
    Unclassified,
    AttributionDivergence,
    Excellent,
    Healthy,
    NeedsAttention,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevScorecardState {
    pub identifier: String,
    pub commits: u64,
    pub active_days: u64,
    pub merged_prs: u64,
    pub commits_per_active_day: f64,
    pub prs_per_week: f64,
    pub delivered_tickets: usize,
    pub clean_tickets: usize,
    pub bounced_tickets: usize,
    pub first_pass_qa_rate: f64,
    pub score: f64,
    pub attribution_divergence: bool,
    pub decision: DevScorecardDecision,
}

impl DevScorecardState {
    #[must_use]
    pub fn from_score(score: &DevScore) -> Self {
        Self {
            identifier: score.name.clone(),
            commits: score.commits,
            active_days: score.active_days,
            merged_prs: score.merged_prs,
            commits_per_active_day: score.commits_per_active_day,
            prs_per_week: score.prs_per_week,
            delivered_tickets: score.delivered_tickets,
            clean_tickets: score.clean_tickets,
            bounced_tickets: score.bounced_tickets,
            first_pass_qa_rate: score.first_pass_qa_rate,
            score: score.score,
            attribution_divergence: score.attribution_divergence,
            decision: DevScorecardDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DevScorecardGraphRun {
    pub state: DevScorecardState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DevScorecardState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct DevScorecardAuthorization {
    identifier: String,
    decision: DevScorecardDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl DevScorecardAuthorization {
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    #[must_use]
    pub fn decision(&self) -> DevScorecardDecision {
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

impl DevScorecardGraphRun {
    #[must_use]
    pub fn scorecard_authorization(&self) -> Result<Option<DevScorecardAuthorization>, String> {
        if self.state.decision == DevScorecardDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "dev_scorecard",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(DevScorecardAuthorization {
            identifier: self.state.identifier.clone(),
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ATTRIBUTION_DIVERGENCE: &str = "attribution_divergence";
const EXCELLENT: &str = "excellent";
const HEALTHY: &str = "healthy";
const NEEDS_ATTENTION: &str = "needs_attention";

pub type DevScorecardGraph = CompilationResult<DevScorecardState>;

#[must_use]
pub fn dev_scorecard_decision_label(decision: DevScorecardDecision) -> &'static str {
    match decision {
        DevScorecardDecision::Unclassified => "unclassified",
        DevScorecardDecision::AttributionDivergence => "attribution-divergence",
        DevScorecardDecision::Excellent => "excellent",
        DevScorecardDecision::Healthy => "healthy",
        DevScorecardDecision::NeedsAttention => "needs-attention",
    }
}

fn expected_decision(state: &DevScorecardState) -> DevScorecardDecision {
    if state.attribution_divergence {
        DevScorecardDecision::AttributionDivergence
    } else if state.score >= 85.0 {
        DevScorecardDecision::Excellent
    } else if state.score >= 70.0 {
        DevScorecardDecision::Healthy
    } else {
        DevScorecardDecision::NeedsAttention
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "dev_scorecard")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn dev_scorecard_state_schema() -> StateSchema<DevScorecardState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "commits",
                "active_days",
                "merged_prs",
                "commits_per_active_day",
                "prs_per_week",
                "delivered_tickets",
                "clean_tickets",
                "bounced_tickets",
                "first_pass_qa_rate",
                "score",
                "attribution_divergence",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "commits": { "type": "integer", "minimum": 0 },
                "active_days": { "type": "integer", "minimum": 0 },
                "merged_prs": { "type": "integer", "minimum": 0 },
                "commits_per_active_day": { "type": "number", "minimum": 0 },
                "prs_per_week": { "type": "number", "minimum": 0 },
                "delivered_tickets": { "type": "integer", "minimum": 0 },
                "clean_tickets": { "type": "integer", "minimum": 0 },
                "bounced_tickets": { "type": "integer", "minimum": 0 },
                "first_pass_qa_rate": { "type": "number", "minimum": 0, "maximum": 1 },
                "score": { "type": "number", "minimum": 0, "maximum": 100 },
                "attribution_divergence": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "AttributionDivergence",
                        "Excellent",
                        "Healthy",
                        "NeedsAttention"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "dev_scorecard",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &DevScorecardState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "dev_scorecard identifier must not be empty".to_string(),
                ));
            }
            let finite_scores = [
                state.commits_per_active_day,
                state.prs_per_week,
                state.first_pass_qa_rate,
                state.score,
            ];
            if finite_scores.iter().any(|score| !score.is_finite()) {
                return Err(StateError::ValidationFailed(
                    "dev_scorecard numeric metrics must be finite".to_string(),
                ));
            }
            if !(0.0..=1.0).contains(&state.first_pass_qa_rate) {
                return Err(StateError::ValidationFailed(
                    "dev_scorecard first_pass_qa_rate must be between 0 and 1".to_string(),
                ));
            }
            if !(0.0..=100.0).contains(&state.score) {
                return Err(StateError::ValidationFailed(
                    "dev_scorecard score must be between 0 and 100".to_string(),
                ));
            }
            if state.clean_tickets + state.bounced_tickets > state.delivered_tickets {
                return Err(StateError::ValidationFailed(
                    "dev_scorecard clean+bounced tickets cannot exceed delivered tickets"
                        .to_string(),
                ));
            }
            if state.decision != DevScorecardDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "dev_scorecard terminal decision must match score and attribution inputs"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_dev_scorecard_graph() -> Result<DevScorecardGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("dev_scorecard").await?;
    build_dev_scorecard_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_dev_scorecard_graph_with_ephemeral_sqlite() -> Result<DevScorecardGraph, String> {
    build_dev_scorecard_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_dev_scorecard_graph_with_database_path(
    database_path: &str,
) -> Result<DevScorecardGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_dev_scorecard_graph_with_checkpointer(checkpointer).await
}

async fn build_dev_scorecard_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<DevScorecardGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = dev_scorecard_state_schema();
    let builder = StateGraphBuilder::<DevScorecardState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: DevScorecardState| async move {
                emit_decision_node_event("dev_scorecard", CLASSIFY, &s.identifier)?;
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
            ATTRIBUTION_DIVERGENCE,
            |s: DevScorecardState| async move {
                emit_decision_node_event("dev_scorecard", ATTRIBUTION_DIVERGENCE, &s.identifier)?;
                let mut next = s;
                next.decision = DevScorecardDecision::AttributionDivergence;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ATTRIBUTION_DIVERGENCE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            EXCELLENT,
            |s: DevScorecardState| async move {
                emit_decision_node_event("dev_scorecard", EXCELLENT, &s.identifier)?;
                let mut next = s;
                next.decision = DevScorecardDecision::Excellent;
                Ok::<_, NodeError>(next)
            },
            node_config(
                EXCELLENT,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            HEALTHY,
            |s: DevScorecardState| async move {
                emit_decision_node_event("dev_scorecard", HEALTHY, &s.identifier)?;
                let mut next = s;
                next.decision = DevScorecardDecision::Healthy;
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
            NEEDS_ATTENTION,
            |s: DevScorecardState| async move {
                emit_decision_node_event("dev_scorecard", NEEDS_ATTENTION, &s.identifier)?;
                let mut next = s;
                next.decision = DevScorecardDecision::NeedsAttention;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NEEDS_ATTENTION,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &DevScorecardState| {
            match expected_decision(s) {
                DevScorecardDecision::AttributionDivergence => ATTRIBUTION_DIVERGENCE.into(),
                DevScorecardDecision::Excellent => EXCELLENT.into(),
                DevScorecardDecision::Healthy => HEALTHY.into(),
                DevScorecardDecision::NeedsAttention => NEEDS_ATTENTION.into(),
                DevScorecardDecision::Unclassified => NEEDS_ATTENTION.into(),
            }
        })
        .add_edge(ATTRIBUTION_DIVERGENCE, END)
        .add_edge(EXCELLENT, END)
        .add_edge(HEALTHY, END)
        .add_edge(NEEDS_ATTENTION, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_dev_scorecard_decision_report(
    compiled: &DevScorecardGraph,
    state: DevScorecardState,
) -> Result<DevScorecardGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "dev_scorecard",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "dev_scorecard", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "dev_scorecard",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(DevScorecardGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: dev_scorecard_graph_topology(compiled)?,
    })
}

pub fn dev_scorecard_graph_topology(
    compiled: &DevScorecardGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("dev_scorecard", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(name: &str, score: f64, attribution_divergence: bool) -> DevScore {
        DevScore {
            name: name.to_string(),
            commits: 40,
            active_days: 10,
            merged_prs: 12,
            commits_per_active_day: 4.0,
            prs_per_week: 8.4,
            delivered_tickets: 6,
            clean_tickets: 5,
            bounced_tickets: 1,
            first_pass_qa_rate: 0.83,
            score,
            attribution_divergence,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_attribution_divergence() {
        let graph = build_dev_scorecard_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = DevScorecardState::from_score(&score("Rene", 91.0, true));
        let run = run_dev_scorecard_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(
            run.state.decision,
            DevScorecardDecision::AttributionDivergence
        );
        assert_eq!(run.topology.graph, "dev_scorecard");
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
        let authorization = run
            .scorecard_authorization()
            .expect("scorecard decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.identifier(), "Rene");
        assert_eq!(
            authorization.decision(),
            DevScorecardDecision::AttributionDivergence
        );
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_excellent_scorecard() {
        let graph = build_dev_scorecard_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = DevScorecardState::from_score(&score("Ada", 88.0, false));
        let run = run_dev_scorecard_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, DevScorecardDecision::Excellent);
        assert_eq!(
            run.scorecard_authorization()
                .expect("scorecard decision should have checkpoint authorization")
                .expect("authorization")
                .decision(),
            DevScorecardDecision::Excellent
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_invalid_score() {
        let graph = build_dev_scorecard_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = DevScorecardState::from_score(&score("Bad", 120.0, false));
        let err = run_dev_scorecard_decision_report(&graph, state)
            .await
            .expect_err("invalid score should fail schema validation");
        assert!(err.contains("score"));
    }
}
