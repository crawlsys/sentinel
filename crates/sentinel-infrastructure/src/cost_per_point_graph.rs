//! Graph-backed cost-per-point report classification.
//!
//! The SEN-13 scanner joins token cost with Linear estimates. This graph
//! validates the aggregate math and emits a checkpointed sizing-curve verdict.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::cost_per_point::{
    BucketStats, CostPerPointReport, DRIFT_ALARM_THRESHOLD,
};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CostPerPointDecision {
    #[default]
    Unclassified,
    NoData,
    MissingEstimateData,
    DriftAlarm,
    CoverageRisk,
    InsufficientCurveBaseline,
    HealthyCurve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostPerPointBucketState {
    pub bucket: u8,
    pub n: usize,
    pub cost_p25: f64,
    pub cost_p50: f64,
    pub cost_p75: f64,
    pub cost_p90: f64,
    pub tokens_p25: f64,
    pub tokens_p50: f64,
    pub tokens_p75: f64,
    pub tokens_p90: f64,
}

impl CostPerPointBucketState {
    #[must_use]
    pub fn from_bucket(bucket: u8, stats: &BucketStats) -> Self {
        Self {
            bucket,
            n: stats.n,
            cost_p25: stats.cost_p25,
            cost_p50: stats.cost_p50,
            cost_p75: stats.cost_p75,
            cost_p90: stats.cost_p90,
            tokens_p25: stats.tokens_p25,
            tokens_p50: stats.tokens_p50,
            tokens_p75: stats.tokens_p75,
            tokens_p90: stats.tokens_p90,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostPerPointState {
    pub identifier: String,
    pub tickets_analyzed: usize,
    pub tickets_with_estimate: usize,
    pub estimate_coverage_fraction: f64,
    pub buckets_count: usize,
    pub samples_in_buckets: usize,
    pub drift_ratio_high_vs_low: Option<f64>,
    pub drift_alarm_threshold: f64,
    pub drift_alarm: bool,
    pub buckets: Vec<CostPerPointBucketState>,
    pub decision: CostPerPointDecision,
}

impl CostPerPointState {
    #[must_use]
    pub fn from_report(report: &CostPerPointReport) -> Self {
        let buckets: Vec<CostPerPointBucketState> = report
            .buckets
            .iter()
            .map(|(bucket, stats)| CostPerPointBucketState::from_bucket(*bucket, stats))
            .collect();
        let samples_in_buckets = buckets.iter().map(|bucket| bucket.n).sum();
        let estimate_coverage_fraction = if report.tickets_analyzed == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let with_estimate = report.tickets_with_estimate as f64;
            #[allow(clippy::cast_precision_loss)]
            let analyzed = report.tickets_analyzed as f64;
            with_estimate / analyzed
        };
        Self {
            identifier: "aggregate".to_string(),
            tickets_analyzed: report.tickets_analyzed,
            tickets_with_estimate: report.tickets_with_estimate,
            estimate_coverage_fraction,
            buckets_count: buckets.len(),
            samples_in_buckets,
            drift_ratio_high_vs_low: report.drift_ratio_high_vs_low,
            drift_alarm_threshold: DRIFT_ALARM_THRESHOLD,
            drift_alarm: report.drift_alarm,
            buckets,
            decision: CostPerPointDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CostPerPointGraphRun {
    pub state: CostPerPointState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<CostPerPointState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct CostPerPointAuthorization {
    decision: CostPerPointDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl CostPerPointAuthorization {
    #[must_use]
    pub fn decision(&self) -> CostPerPointDecision {
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

impl CostPerPointGraphRun {
    #[must_use]
    pub fn cost_per_point_authorization(
        &self,
    ) -> Result<Option<CostPerPointAuthorization>, String> {
        if self.state.decision == CostPerPointDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "cost_per_point",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(CostPerPointAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const MISSING_ESTIMATE_DATA: &str = "missing_estimate_data";
const DRIFT_ALARM: &str = "drift_alarm";
const COVERAGE_RISK: &str = "coverage_risk";
const INSUFFICIENT_CURVE_BASELINE: &str = "insufficient_curve_baseline";
const HEALTHY_CURVE: &str = "healthy_curve";

pub type CostPerPointGraph = CompilationResult<CostPerPointState>;

#[must_use]
pub fn cost_per_point_decision_label(decision: CostPerPointDecision) -> &'static str {
    match decision {
        CostPerPointDecision::Unclassified => "unclassified",
        CostPerPointDecision::NoData => "no-data",
        CostPerPointDecision::MissingEstimateData => "missing-estimate-data",
        CostPerPointDecision::DriftAlarm => "drift-alarm",
        CostPerPointDecision::CoverageRisk => "coverage-risk",
        CostPerPointDecision::InsufficientCurveBaseline => "insufficient-curve-baseline",
        CostPerPointDecision::HealthyCurve => "healthy-curve",
    }
}

fn expected_decision(state: &CostPerPointState) -> CostPerPointDecision {
    if state.tickets_analyzed == 0 {
        CostPerPointDecision::NoData
    } else if state.tickets_with_estimate == 0 {
        CostPerPointDecision::MissingEstimateData
    } else if state.drift_alarm {
        CostPerPointDecision::DriftAlarm
    } else if state.estimate_coverage_fraction < 0.80 {
        CostPerPointDecision::CoverageRisk
    } else if state.drift_ratio_high_vs_low.is_none() {
        CostPerPointDecision::InsufficientCurveBaseline
    } else {
        CostPerPointDecision::HealthyCurve
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "cost_per_point")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.000_001_f64.max(right.abs() * 0.000_001)
}

fn cost_per_point_state_schema() -> StateSchema<CostPerPointState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tickets_analyzed",
                "tickets_with_estimate",
                "estimate_coverage_fraction",
                "buckets_count",
                "samples_in_buckets",
                "drift_ratio_high_vs_low",
                "drift_alarm_threshold",
                "drift_alarm",
                "buckets",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tickets_analyzed": { "type": "integer", "minimum": 0 },
                "tickets_with_estimate": { "type": "integer", "minimum": 0 },
                "estimate_coverage_fraction": { "type": "number", "minimum": 0, "maximum": 1 },
                "buckets_count": { "type": "integer", "minimum": 0 },
                "samples_in_buckets": { "type": "integer", "minimum": 0 },
                "drift_ratio_high_vs_low": {
                    "anyOf": [
                        { "type": "number", "minimum": 0 },
                        { "type": "null" }
                    ]
                },
                "drift_alarm_threshold": { "type": "number", "exclusiveMinimum": 0 },
                "drift_alarm": { "type": "boolean" },
                "buckets": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "bucket",
                            "n",
                            "cost_p25",
                            "cost_p50",
                            "cost_p75",
                            "cost_p90",
                            "tokens_p25",
                            "tokens_p50",
                            "tokens_p75",
                            "tokens_p90"
                        ],
                        "properties": {
                            "bucket": { "type": "integer", "minimum": 1 },
                            "n": { "type": "integer", "minimum": 1 },
                            "cost_p25": { "type": "number", "minimum": 0 },
                            "cost_p50": { "type": "number", "minimum": 0 },
                            "cost_p75": { "type": "number", "minimum": 0 },
                            "cost_p90": { "type": "number", "minimum": 0 },
                            "tokens_p25": { "type": "number", "minimum": 0 },
                            "tokens_p50": { "type": "number", "minimum": 0 },
                            "tokens_p75": { "type": "number", "minimum": 0 },
                            "tokens_p90": { "type": "number", "minimum": 0 }
                        }
                    }
                },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "MissingEstimateData",
                        "DriftAlarm",
                        "CoverageRisk",
                        "InsufficientCurveBaseline",
                        "HealthyCurve"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "cost_per_point",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &CostPerPointState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "cost_per_point identifier must not be empty".to_string(),
                ));
            }
            if state.tickets_with_estimate > state.tickets_analyzed {
                return Err(StateError::ValidationFailed(
                    "cost_per_point tickets_with_estimate cannot exceed tickets_analyzed"
                        .to_string(),
                ));
            }
            if !state.estimate_coverage_fraction.is_finite()
                || !(0.0..=1.0).contains(&state.estimate_coverage_fraction)
                || !state.drift_alarm_threshold.is_finite()
                || state.drift_alarm_threshold <= 0.0
            {
                return Err(StateError::ValidationFailed(
                    "cost_per_point coverage and threshold values must be finite".to_string(),
                ));
            }
            #[allow(clippy::cast_precision_loss)]
            let expected_coverage = if state.tickets_analyzed == 0 {
                0.0
            } else {
                state.tickets_with_estimate as f64 / state.tickets_analyzed as f64
            };
            if !approx_eq(state.estimate_coverage_fraction, expected_coverage) {
                return Err(StateError::ValidationFailed(
                    "cost_per_point coverage fraction must match ticket counts".to_string(),
                ));
            }
            if state.buckets_count != state.buckets.len() {
                return Err(StateError::ValidationFailed(
                    "cost_per_point buckets_count must equal bucket row count".to_string(),
                ));
            }
            let samples: usize = state.buckets.iter().map(|bucket| bucket.n).sum();
            if samples != state.samples_in_buckets || samples != state.tickets_with_estimate {
                return Err(StateError::ValidationFailed(
                    "cost_per_point bucket sample totals must match estimated ticket count"
                        .to_string(),
                ));
            }
            for bucket in &state.buckets {
                validate_bucket(bucket)?;
            }
            validate_drift_ratio(state)?;
            if state.decision != CostPerPointDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "cost_per_point terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

fn validate_bucket(bucket: &CostPerPointBucketState) -> Result<(), StateError> {
    let values = [
        bucket.cost_p25,
        bucket.cost_p50,
        bucket.cost_p75,
        bucket.cost_p90,
        bucket.tokens_p25,
        bucket.tokens_p50,
        bucket.tokens_p75,
        bucket.tokens_p90,
    ];
    if values
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(StateError::ValidationFailed(
            "cost_per_point bucket values must be finite and non-negative".to_string(),
        ));
    }
    if bucket.cost_p25 > bucket.cost_p50
        || bucket.cost_p50 > bucket.cost_p75
        || bucket.cost_p75 > bucket.cost_p90
        || bucket.tokens_p25 > bucket.tokens_p50
        || bucket.tokens_p50 > bucket.tokens_p75
        || bucket.tokens_p75 > bucket.tokens_p90
    {
        return Err(StateError::ValidationFailed(
            "cost_per_point bucket percentiles must be monotonic".to_string(),
        ));
    }
    Ok(())
}

fn validate_drift_ratio(state: &CostPerPointState) -> Result<(), StateError> {
    let bucket_2 = state.buckets.iter().find(|bucket| bucket.bucket == 2);
    let bucket_8 = state.buckets.iter().find(|bucket| bucket.bucket == 8);
    let expected_ratio = match (bucket_8, bucket_2) {
        (Some(high), Some(low)) if low.cost_p50 > 0.0 => Some(high.cost_p50 / low.cost_p50),
        _ => None,
    };
    match (state.drift_ratio_high_vs_low, expected_ratio) {
        (Some(actual), Some(expected)) if actual.is_finite() && approx_eq(actual, expected) => {}
        (None, None) => {}
        (Some(_), Some(_)) => {
            return Err(StateError::ValidationFailed(
                "cost_per_point drift ratio must match bucket 8 / bucket 2 medians".to_string(),
            ));
        }
        _ => {
            return Err(StateError::ValidationFailed(
                "cost_per_point drift ratio presence must match bucket evidence".to_string(),
            ));
        }
    }
    let expected_alarm = state
        .drift_ratio_high_vs_low
        .is_some_and(|ratio| ratio > state.drift_alarm_threshold);
    if state.drift_alarm != expected_alarm {
        return Err(StateError::ValidationFailed(
            "cost_per_point drift_alarm must match ratio threshold".to_string(),
        ));
    }
    Ok(())
}

pub async fn build_cost_per_point_graph() -> Result<CostPerPointGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("cost_per_point").await?;
    build_cost_per_point_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_cost_per_point_graph_with_ephemeral_sqlite() -> Result<CostPerPointGraph, String> {
    build_cost_per_point_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_cost_per_point_graph_with_database_path(
    database_path: &str,
) -> Result<CostPerPointGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_cost_per_point_graph_with_checkpointer(checkpointer).await
}

async fn build_cost_per_point_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<CostPerPointGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = cost_per_point_state_schema();
    let builder = StateGraphBuilder::<CostPerPointState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: CostPerPointState| async move {
                emit_decision_node_event("cost_per_point", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_DATA,
            |s: CostPerPointState| async move {
                emit_decision_node_event("cost_per_point", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = CostPerPointDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_DATA, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            MISSING_ESTIMATE_DATA,
            |s: CostPerPointState| async move {
                emit_decision_node_event("cost_per_point", MISSING_ESTIMATE_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = CostPerPointDecision::MissingEstimateData;
                Ok::<_, NodeError>(next)
            },
            node_config(
                MISSING_ESTIMATE_DATA,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DRIFT_ALARM,
            |s: CostPerPointState| async move {
                emit_decision_node_event("cost_per_point", DRIFT_ALARM, &s.identifier)?;
                let mut next = s;
                next.decision = CostPerPointDecision::DriftAlarm;
                Ok::<_, NodeError>(next)
            },
            node_config(DRIFT_ALARM, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            COVERAGE_RISK,
            |s: CostPerPointState| async move {
                emit_decision_node_event("cost_per_point", COVERAGE_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = CostPerPointDecision::CoverageRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(COVERAGE_RISK, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            INSUFFICIENT_CURVE_BASELINE,
            |s: CostPerPointState| async move {
                emit_decision_node_event(
                    "cost_per_point",
                    INSUFFICIENT_CURVE_BASELINE,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = CostPerPointDecision::InsufficientCurveBaseline;
                Ok::<_, NodeError>(next)
            },
            node_config(
                INSUFFICIENT_CURVE_BASELINE,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            HEALTHY_CURVE,
            |s: CostPerPointState| async move {
                emit_decision_node_event("cost_per_point", HEALTHY_CURVE, &s.identifier)?;
                let mut next = s;
                next.decision = CostPerPointDecision::HealthyCurve;
                Ok::<_, NodeError>(next)
            },
            node_config(HEALTHY_CURVE, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &CostPerPointState| {
            match expected_decision(s) {
                CostPerPointDecision::NoData => NO_DATA.into(),
                CostPerPointDecision::MissingEstimateData => MISSING_ESTIMATE_DATA.into(),
                CostPerPointDecision::DriftAlarm => DRIFT_ALARM.into(),
                CostPerPointDecision::CoverageRisk => COVERAGE_RISK.into(),
                CostPerPointDecision::InsufficientCurveBaseline => {
                    INSUFFICIENT_CURVE_BASELINE.into()
                }
                CostPerPointDecision::HealthyCurve => HEALTHY_CURVE.into(),
                CostPerPointDecision::Unclassified => NO_DATA.into(),
            }
        })
        .add_edge(NO_DATA, END)
        .add_edge(MISSING_ESTIMATE_DATA, END)
        .add_edge(DRIFT_ALARM, END)
        .add_edge(COVERAGE_RISK, END)
        .add_edge(INSUFFICIENT_CURVE_BASELINE, END)
        .add_edge(HEALTHY_CURVE, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_cost_per_point_decision_report(
    compiled: &CostPerPointGraph,
    state: CostPerPointState,
) -> Result<CostPerPointGraphRun, String> {
    let thread_id =
        crate::decision_graph_store::run_thread_id("cost_per_point", "aggregate", &state)?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "cost_per_point", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "cost_per_point",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(CostPerPointGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: cost_per_point_graph_topology(compiled)?,
    })
}

pub fn cost_per_point_graph_topology(
    compiled: &CostPerPointGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("cost_per_point", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::cost_per_point::CostPerPointReport;
    use std::collections::BTreeMap;

    fn bucket(n: usize, cost_p50: f64) -> BucketStats {
        BucketStats {
            n,
            cost_p25: cost_p50,
            cost_p50,
            cost_p75: cost_p50,
            cost_p90: cost_p50,
            tokens_p25: 1_000.0,
            tokens_p50: 1_000.0,
            tokens_p75: 1_000.0,
            tokens_p90: 1_000.0,
        }
    }

    fn report(drift_alarm: bool) -> CostPerPointReport {
        let mut buckets = BTreeMap::new();
        buckets.insert(2, bucket(5, 1.0));
        buckets.insert(8, bucket(5, if drift_alarm { 6.0 } else { 4.0 }));
        CostPerPointReport {
            tickets_analyzed: 10,
            tickets_with_estimate: 10,
            buckets,
            drift_ratio_high_vs_low: Some(if drift_alarm { 6.0 } else { 4.0 }),
            drift_alarm,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_healthy_curve_report() {
        let graph = build_cost_per_point_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = CostPerPointState::from_report(&report(false));
        let run = run_cost_per_point_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, CostPerPointDecision::HealthyCurve);
        assert_eq!(run.topology.graph, "cost_per_point");
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
            .cost_per_point_authorization()
            .expect("cost-per-point decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), CostPerPointDecision::HealthyCurve);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_prioritizes_drift_alarm() {
        let graph = build_cost_per_point_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = CostPerPointState::from_report(&report(true));
        let run = run_cost_per_point_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, CostPerPointDecision::DriftAlarm);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_drift_math() {
        let graph = build_cost_per_point_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = CostPerPointState::from_report(&report(false));
        state.drift_ratio_high_vs_low = Some(99.0);

        let err = run_cost_per_point_decision_report(&graph, state)
            .await
            .expect_err("broken drift math should fail schema validation");
        assert!(err.contains("drift ratio"));
    }
}
