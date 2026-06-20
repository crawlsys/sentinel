//! Graph-backed eval run classification.
//!
//! A12 eval scoring still uses the configured `EvalScorerPort` to produce
//! per-axis scores. This graph validates the aggregate run result and emits a
//! checkpointed benchmark verdict so eval runs live on the same durable
//! LangGraph authority substrate as Sentinel's operational metrics.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_domain::eval::EvalRunResult;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EvalRunDecision {
    #[default]
    Unclassified,
    NoCases,
    ScoringErrors,
    Failing,
    Borderline,
    Passing,
    Strong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalRunState {
    pub identifier: String,
    pub total_cases: u64,
    pub successful_cases: u64,
    pub errored_cases: u64,
    pub mean_composite: Option<f64>,
    pub pass_rate_075: f64,
    pub axis_mean_count: usize,
    pub min_axis_mean: Option<f64>,
    pub decision: EvalRunDecision,
}

impl EvalRunState {
    #[must_use]
    pub fn from_run(run: &EvalRunResult) -> Self {
        let axis_means = run.mean_per_axis();
        let min_axis_mean = axis_means
            .iter()
            .map(|(_, mean)| quantize_score(f64::from(*mean)))
            .reduce(f64::min);
        Self {
            identifier: run.run_id.as_str().to_string(),
            total_cases: run.case_results.len() as u64,
            successful_cases: run.successful_case_count() as u64,
            errored_cases: run.errored_case_count() as u64,
            mean_composite: run
                .mean_composite()
                .map(|score| quantize_score(f64::from(score))),
            pass_rate_075: quantize_score(f64::from(run.pass_rate(0.75))),
            axis_mean_count: axis_means.len(),
            min_axis_mean,
            decision: EvalRunDecision::Unclassified,
        }
    }
}

fn quantize_score(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

#[derive(Debug, Clone, Serialize)]
pub struct EvalRunGraphRun {
    pub state: EvalRunState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<EvalRunState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct EvalRunAuthorization {
    decision: EvalRunDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl EvalRunAuthorization {
    #[must_use]
    pub fn decision(&self) -> EvalRunDecision {
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

impl EvalRunGraphRun {
    #[must_use]
    pub fn eval_authorization(&self) -> Result<Option<EvalRunAuthorization>, String> {
        if self.state.decision == EvalRunDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "eval",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(EvalRunAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_CASES: &str = "no_cases";
const SCORING_FAILED: &str = "scoring_failed";
const FAILING: &str = "failing";
const BORDERLINE: &str = "borderline";
const PASSING: &str = "passing";
const STRONG: &str = "strong";

pub type EvalGraph = CompilationResult<EvalRunState>;

#[must_use]
pub fn eval_decision_label(decision: EvalRunDecision) -> &'static str {
    match decision {
        EvalRunDecision::Unclassified => "unclassified",
        EvalRunDecision::NoCases => "no-cases",
        EvalRunDecision::ScoringErrors => "scoring-errors",
        EvalRunDecision::Failing => "failing",
        EvalRunDecision::Borderline => "borderline",
        EvalRunDecision::Passing => "passing",
        EvalRunDecision::Strong => "strong",
    }
}

fn expected_decision(state: &EvalRunState) -> EvalRunDecision {
    if state.total_cases == 0 {
        return EvalRunDecision::NoCases;
    }
    if state.errored_cases > 0 || state.successful_cases == 0 {
        return EvalRunDecision::ScoringErrors;
    }
    let mean = state.mean_composite.unwrap_or(0.0);
    if mean < 0.60 {
        EvalRunDecision::Failing
    } else if mean < 0.75 {
        EvalRunDecision::Borderline
    } else if state.pass_rate_075 >= 0.90 && state.min_axis_mean.unwrap_or(0.0) >= 0.80 {
        EvalRunDecision::Strong
    } else {
        EvalRunDecision::Passing
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "eval")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn optional_score_schema() -> serde_json::Value {
    serde_json::json!({
        "anyOf": [
            { "type": "number", "minimum": 0, "maximum": 1 },
            { "type": "null" }
        ]
    })
}

fn finite_score(value: f64, field: &str) -> Result<(), StateError> {
    if !value.is_finite() {
        return Err(StateError::ValidationFailed(format!(
            "eval {field} must be finite"
        )));
    }
    if !(0.0..=1.0).contains(&value) {
        return Err(StateError::ValidationFailed(format!(
            "eval {field} must be in [0, 1]"
        )));
    }
    Ok(())
}

fn eval_state_schema() -> StateSchema<EvalRunState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "total_cases",
                "successful_cases",
                "errored_cases",
                "mean_composite",
                "pass_rate_075",
                "axis_mean_count",
                "min_axis_mean",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "total_cases": { "type": "integer", "minimum": 0 },
                "successful_cases": { "type": "integer", "minimum": 0 },
                "errored_cases": { "type": "integer", "minimum": 0 },
                "mean_composite": optional_score_schema(),
                "pass_rate_075": { "type": "number", "minimum": 0, "maximum": 1 },
                "axis_mean_count": { "type": "integer", "minimum": 0 },
                "min_axis_mean": optional_score_schema(),
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoCases",
                        "ScoringErrors",
                        "Failing",
                        "Borderline",
                        "Passing",
                        "Strong"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "eval",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &EvalRunState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "eval identifier must not be empty".to_string(),
                ));
            }
            if state.total_cases != state.successful_cases + state.errored_cases {
                return Err(StateError::ValidationFailed(
                    "eval total_cases must equal successful_cases+errored_cases".to_string(),
                ));
            }
            finite_score(state.pass_rate_075, "pass_rate_075")?;
            match (state.successful_cases, state.mean_composite) {
                (0, None) => {}
                (0, Some(_)) => {
                    return Err(StateError::ValidationFailed(
                        "eval mean_composite must be null with zero successful cases".to_string(),
                    ));
                }
                (_, Some(score)) => finite_score(score, "mean_composite")?,
                (_, None) => {
                    return Err(StateError::ValidationFailed(
                        "eval mean_composite is required when successful cases exist".to_string(),
                    ));
                }
            }
            match (state.axis_mean_count, state.min_axis_mean) {
                (0, None) => {}
                (0, Some(_)) => {
                    return Err(StateError::ValidationFailed(
                        "eval min_axis_mean must be null when no axis means exist".to_string(),
                    ));
                }
                (_, Some(score)) => finite_score(score, "min_axis_mean")?,
                (_, None) => {
                    return Err(StateError::ValidationFailed(
                        "eval min_axis_mean is required when axis means exist".to_string(),
                    ));
                }
            }
            if state.decision != EvalRunDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "eval terminal decision must match run aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_eval_graph() -> Result<EvalGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("eval").await?;
    build_eval_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_eval_graph_with_ephemeral_sqlite() -> Result<EvalGraph, String> {
    build_eval_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_eval_graph_with_database_path(database_path: &str) -> Result<EvalGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_eval_graph_with_checkpointer(checkpointer).await
}

async fn build_eval_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<EvalGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = eval_state_schema();
    let builder = StateGraphBuilder::<EvalRunState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", CLASSIFY, &s.identifier)?;
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
            NO_CASES,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", NO_CASES, &s.identifier)?;
                let mut next = s;
                next.decision = EvalRunDecision::NoCases;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NO_CASES,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            SCORING_FAILED,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", SCORING_FAILED, &s.identifier)?;
                let mut next = s;
                next.decision = EvalRunDecision::ScoringErrors;
                Ok::<_, NodeError>(next)
            },
            node_config(
                SCORING_FAILED,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            FAILING,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", FAILING, &s.identifier)?;
                let mut next = s;
                next.decision = EvalRunDecision::Failing;
                Ok::<_, NodeError>(next)
            },
            node_config(
                FAILING,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BORDERLINE,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", BORDERLINE, &s.identifier)?;
                let mut next = s;
                next.decision = EvalRunDecision::Borderline;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BORDERLINE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            PASSING,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", PASSING, &s.identifier)?;
                let mut next = s;
                next.decision = EvalRunDecision::Passing;
                Ok::<_, NodeError>(next)
            },
            node_config(
                PASSING,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            STRONG,
            |s: EvalRunState| async move {
                emit_decision_node_event("eval", STRONG, &s.identifier)?;
                let mut next = s;
                next.decision = EvalRunDecision::Strong;
                Ok::<_, NodeError>(next)
            },
            node_config(
                STRONG,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &EvalRunState| match expected_decision(s) {
            EvalRunDecision::NoCases => NO_CASES.into(),
            EvalRunDecision::ScoringErrors => SCORING_FAILED.into(),
            EvalRunDecision::Failing => FAILING.into(),
            EvalRunDecision::Borderline => BORDERLINE.into(),
            EvalRunDecision::Passing => PASSING.into(),
            EvalRunDecision::Strong => STRONG.into(),
            EvalRunDecision::Unclassified => NO_CASES.into(),
        })
        .add_edge(NO_CASES, END)
        .add_edge(SCORING_FAILED, END)
        .add_edge(FAILING, END)
        .add_edge(BORDERLINE, END)
        .add_edge(PASSING, END)
        .add_edge(STRONG, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_eval_decision_report(
    compiled: &EvalGraph,
    state: EvalRunState,
) -> Result<EvalRunGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "eval",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(compiled, &thread_id, "eval", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "eval",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(EvalRunGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: eval_graph_topology(compiled)?,
    })
}

pub fn eval_graph_topology(compiled: &EvalGraph) -> Result<DecisionGraphTopology, String> {
    topology("eval", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::eval::{
        EvalAxis, EvalAxisScore, EvalCaseId, EvalCaseResult, EvalRunId, EvalScore, ScoringRubric,
    };

    fn scored_case(case_id: &str, run_id: &EvalRunId, score_value: f32) -> EvalCaseResult {
        let rubric = ScoringRubric::ba_default();
        let axis_scores = vec![
            EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, score_value, 1.0),
            EvalAxisScore::new(EvalAxis::RequirementsCoverage, score_value, 1.0),
            EvalAxisScore::new(EvalAxis::AlternativesSeriousness, score_value, 1.0),
            EvalAxisScore::new(EvalAxis::TonalCalibration, score_value, 1.0),
            EvalAxisScore::new(EvalAxis::OutcomeRealism, score_value, 2.0),
            EvalAxisScore::new(EvalAxis::StakeholderFit, score_value, 1.0),
        ];
        let case_id = EvalCaseId::new(case_id).unwrap();
        let score = EvalScore::new(case_id.clone(), run_id.clone(), axis_scores, &rubric);
        EvalCaseResult {
            case_id,
            run_id: run_id.clone(),
            candidate_output: "candidate".to_string(),
            score: Some(score),
            timing_ms: 10,
            completed_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            error: None,
        }
    }

    fn errored_case(case_id: &str, run_id: &EvalRunId) -> EvalCaseResult {
        EvalCaseResult {
            case_id: EvalCaseId::new(case_id).unwrap(),
            run_id: run_id.clone(),
            candidate_output: String::new(),
            score: None,
            timing_ms: 5,
            completed_at: Utc.timestamp_opt(1_700_000_001, 0).unwrap(),
            error: Some("candidate missing".to_string()),
        }
    }

    fn run_result(run_id: &str, scores: &[f32], errors: usize) -> EvalRunResult {
        let run_id = EvalRunId::new(run_id).unwrap();
        let mut case_results = Vec::new();
        for (idx, score) in scores.iter().copied().enumerate() {
            case_results.push(scored_case(&format!("case-{idx}"), &run_id, score));
        }
        for idx in 0..errors {
            case_results.push(errored_case(&format!("error-{idx}"), &run_id));
        }
        EvalRunResult {
            run_id,
            started_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            completed_at: Utc.timestamp_opt(1_700_000_010, 0).unwrap(),
            case_results,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_strong_eval_run() {
        let graph = build_eval_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = EvalRunState::from_run(&run_result("eval-strong", &[0.92, 0.88], 0));
        let run = run_eval_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, EvalRunDecision::Strong);
        assert_eq!(run.topology.graph, "eval");
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
            .eval_authorization()
            .expect("eval decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), EvalRunDecision::Strong);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_prioritizes_scoring_errors() {
        let graph = build_eval_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = EvalRunState::from_run(&run_result("eval-errors", &[0.95], 1));
        let run = run_eval_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, EvalRunDecision::ScoringErrors);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_case_counts() {
        let graph = build_eval_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = EvalRunState::from_run(&run_result("eval-broken", &[0.80], 0));
        state.total_cases = 99;
        let err = run_eval_decision_report(&graph, state)
            .await
            .expect_err("broken case counts should fail schema validation");
        assert!(err.contains("total_cases"));
    }
}
