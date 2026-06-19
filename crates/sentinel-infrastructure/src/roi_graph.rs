//! Graph-backed ROI report classification.
//!
//! The ROI scanner joins token cost and estimate data. This graph validates
//! the aggregate math and emits a checkpointed financial verdict for the CLI.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::roi::{RoiReport, RoiWindow};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RoiDecision {
    #[default]
    Unclassified,
    NoData,
    MissingEstimateData,
    PositiveReturn,
    BreakEven,
    NegativeReturn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoiWindowState {
    pub label: String,
    pub window_days: Option<u32>,
    pub tickets_shipped: usize,
    pub points_shipped: f64,
    pub claude_cost_usd: f64,
    pub claude_cost_per_point: f64,
    pub human_cost_usd: f64,
    pub roi_ratio: f64,
    pub projected_annual_savings_usd: f64,
    pub estimate_data_available: bool,
}

impl RoiWindowState {
    #[must_use]
    pub fn from_window(window: &RoiWindow) -> Self {
        Self {
            label: window.label.clone(),
            window_days: window.window_days,
            tickets_shipped: window.tickets_shipped,
            points_shipped: window.points_shipped,
            claude_cost_usd: window.claude_cost_usd,
            claude_cost_per_point: window.claude_cost_per_point,
            human_cost_usd: window.human_cost_usd,
            roi_ratio: window.roi_ratio,
            projected_annual_savings_usd: window.projected_annual_savings_usd,
            estimate_data_available: window.estimate_data_available,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoiState {
    pub identifier: String,
    pub has_headline: bool,
    pub windows: Vec<RoiWindowState>,
    pub tickets_shipped_total: usize,
    pub claude_cost_usd_total: f64,
    pub human_cost_usd_total: f64,
    pub roi_ratio: f64,
    pub projected_annual_savings_usd: f64,
    pub claude_cost_per_point: f64,
    pub human_cost_per_point: f64,
    pub estimate_data_available: bool,
    pub estimate_note: String,
    pub decision: RoiDecision,
}

impl RoiState {
    #[must_use]
    pub fn from_report(report: &RoiReport) -> Self {
        if let Some(headline) = report.headline.as_ref() {
            let windows = headline
                .windows
                .iter()
                .map(RoiWindowState::from_window)
                .collect();
            Self {
                identifier: "aggregate".to_string(),
                has_headline: true,
                windows,
                tickets_shipped_total: headline.tickets_shipped_total,
                claude_cost_usd_total: headline.claude_cost_usd_total,
                human_cost_usd_total: headline.human_cost_usd_total,
                roi_ratio: headline.roi_ratio,
                projected_annual_savings_usd: headline.projected_annual_savings_usd,
                claude_cost_per_point: headline.claude_cost_per_point,
                human_cost_per_point: headline.human_cost_per_point,
                estimate_data_available: headline.estimate_data_available,
                estimate_note: headline.estimate_note.clone(),
                decision: RoiDecision::Unclassified,
            }
        } else {
            Self {
                identifier: "aggregate".to_string(),
                has_headline: false,
                windows: Vec::new(),
                tickets_shipped_total: 0,
                claude_cost_usd_total: 0.0,
                human_cost_usd_total: 0.0,
                roi_ratio: 0.0,
                projected_annual_savings_usd: 0.0,
                claude_cost_per_point: 0.0,
                human_cost_per_point: sentinel_application::roi::human_baseline_per_point(),
                estimate_data_available: false,
                estimate_note: "no SEN-7 input".to_string(),
                decision: RoiDecision::Unclassified,
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RoiGraphRun {
    pub state: RoiState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<RoiState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct RoiAuthorization {
    decision: RoiDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl RoiAuthorization {
    #[must_use]
    pub fn decision(&self) -> RoiDecision {
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

impl RoiGraphRun {
    #[must_use]
    pub fn roi_authorization(&self) -> Result<Option<RoiAuthorization>, String> {
        if self.state.decision == RoiDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "roi",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(RoiAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const MISSING_ESTIMATE_DATA: &str = "missing_estimate_data";
const POSITIVE_RETURN: &str = "positive_return";
const BREAK_EVEN: &str = "break_even";
const NEGATIVE_RETURN: &str = "negative_return";

pub type RoiGraph = CompilationResult<RoiState>;

#[must_use]
pub fn roi_decision_label(decision: RoiDecision) -> &'static str {
    match decision {
        RoiDecision::Unclassified => "unclassified",
        RoiDecision::NoData => "no-data",
        RoiDecision::MissingEstimateData => "missing-estimate-data",
        RoiDecision::PositiveReturn => "positive-return",
        RoiDecision::BreakEven => "break-even",
        RoiDecision::NegativeReturn => "negative-return",
    }
}

fn expected_decision(state: &RoiState) -> RoiDecision {
    if !state.has_headline || state.tickets_shipped_total == 0 {
        RoiDecision::NoData
    } else if !state.estimate_data_available {
        RoiDecision::MissingEstimateData
    } else {
        let delta = state.human_cost_usd_total - state.claude_cost_usd_total;
        if approx_eq(delta, 0.0) {
            RoiDecision::BreakEven
        } else if delta > 0.0 {
            RoiDecision::PositiveReturn
        } else {
            RoiDecision::NegativeReturn
        }
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "roi")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.000_001_f64.max(right.abs() * 0.000_001)
}

fn non_negative_finite(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

fn roi_state_schema() -> StateSchema<RoiState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "has_headline",
                "windows",
                "tickets_shipped_total",
                "claude_cost_usd_total",
                "human_cost_usd_total",
                "roi_ratio",
                "projected_annual_savings_usd",
                "claude_cost_per_point",
                "human_cost_per_point",
                "estimate_data_available",
                "estimate_note",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "has_headline": { "type": "boolean" },
                "windows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "label",
                            "window_days",
                            "tickets_shipped",
                            "points_shipped",
                            "claude_cost_usd",
                            "claude_cost_per_point",
                            "human_cost_usd",
                            "roi_ratio",
                            "projected_annual_savings_usd",
                            "estimate_data_available"
                        ],
                        "properties": {
                            "label": { "type": "string", "minLength": 1 },
                            "window_days": {
                                "anyOf": [
                                    { "type": "integer", "minimum": 0 },
                                    { "type": "null" }
                                ]
                            },
                            "tickets_shipped": { "type": "integer", "minimum": 0 },
                            "points_shipped": { "type": "number", "minimum": 0 },
                            "claude_cost_usd": { "type": "number", "minimum": 0 },
                            "claude_cost_per_point": { "type": "number", "minimum": 0 },
                            "human_cost_usd": { "type": "number", "minimum": 0 },
                            "roi_ratio": { "type": "number", "minimum": 0 },
                            "projected_annual_savings_usd": { "type": "number" },
                            "estimate_data_available": { "type": "boolean" }
                        }
                    }
                },
                "tickets_shipped_total": { "type": "integer", "minimum": 0 },
                "claude_cost_usd_total": { "type": "number", "minimum": 0 },
                "human_cost_usd_total": { "type": "number", "minimum": 0 },
                "roi_ratio": { "type": "number", "minimum": 0 },
                "projected_annual_savings_usd": { "type": "number" },
                "claude_cost_per_point": { "type": "number", "minimum": 0 },
                "human_cost_per_point": { "type": "number", "minimum": 0 },
                "estimate_data_available": { "type": "boolean" },
                "estimate_note": { "type": "string" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "MissingEstimateData",
                        "PositiveReturn",
                        "BreakEven",
                        "NegativeReturn"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "roi",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &RoiState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "roi identifier must not be empty".to_string(),
                ));
            }
            let headline_numbers = [
                state.claude_cost_usd_total,
                state.human_cost_usd_total,
                state.roi_ratio,
                state.claude_cost_per_point,
                state.human_cost_per_point,
            ];
            if headline_numbers
                .iter()
                .any(|value| !non_negative_finite(*value))
                || !state.projected_annual_savings_usd.is_finite()
            {
                return Err(StateError::ValidationFailed(
                    "roi headline numeric values must be finite and non-negative".to_string(),
                ));
            }
            if !state.has_headline && (!state.windows.is_empty() || state.tickets_shipped_total > 0)
            {
                return Err(StateError::ValidationFailed(
                    "roi no-data state must not carry windows or tickets".to_string(),
                ));
            }
            if state.tickets_shipped_total > 0
                && !state.estimate_data_available
                && state.estimate_note.trim().is_empty()
            {
                return Err(StateError::ValidationFailed(
                    "roi missing estimate data requires an estimate_note".to_string(),
                ));
            }
            validate_ratio(
                "roi headline",
                state.human_cost_usd_total,
                state.claude_cost_usd_total,
                state.roi_ratio,
            )?;
            for window in &state.windows {
                validate_window(window)?;
            }
            if state.has_headline {
                let Some(all_time) = state
                    .windows
                    .iter()
                    .find(|window| window.window_days.is_none())
                else {
                    return Err(StateError::ValidationFailed(
                        "roi headline requires an all-time window".to_string(),
                    ));
                };
                if all_time.tickets_shipped != state.tickets_shipped_total {
                    return Err(StateError::ValidationFailed(
                        "roi headline ticket total must match all-time window".to_string(),
                    ));
                }
                if !approx_eq(all_time.claude_cost_usd, state.claude_cost_usd_total)
                    || !approx_eq(all_time.human_cost_usd, state.human_cost_usd_total)
                    || !approx_eq(all_time.roi_ratio, state.roi_ratio)
                    || !approx_eq(
                        all_time.projected_annual_savings_usd,
                        state.projected_annual_savings_usd,
                    )
                    || !approx_eq(all_time.claude_cost_per_point, state.claude_cost_per_point)
                    || all_time.estimate_data_available != state.estimate_data_available
                {
                    return Err(StateError::ValidationFailed(
                        "roi headline values must match all-time window".to_string(),
                    ));
                }
            }
            if state.decision != RoiDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "roi terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

fn validate_window(window: &RoiWindowState) -> Result<(), StateError> {
    if window.label.trim().is_empty() {
        return Err(StateError::ValidationFailed(
            "roi window label must not be empty".to_string(),
        ));
    }
    let values = [
        window.points_shipped,
        window.claude_cost_usd,
        window.claude_cost_per_point,
        window.human_cost_usd,
        window.roi_ratio,
    ];
    if values.iter().any(|value| !non_negative_finite(*value))
        || !window.projected_annual_savings_usd.is_finite()
    {
        return Err(StateError::ValidationFailed(
            "roi window numeric values must be finite and non-negative".to_string(),
        ));
    }
    if !window.estimate_data_available && window.human_cost_usd > 0.0 {
        return Err(StateError::ValidationFailed(
            "roi window without estimates must not report human cost".to_string(),
        ));
    }
    validate_ratio(
        "roi window",
        window.human_cost_usd,
        window.claude_cost_usd,
        window.roi_ratio,
    )
}

fn validate_ratio(
    label: &str,
    human_cost_usd: f64,
    claude_cost_usd: f64,
    roi_ratio: f64,
) -> Result<(), StateError> {
    let expected_ratio = if claude_cost_usd > 0.0 {
        human_cost_usd / claude_cost_usd
    } else {
        0.0
    };
    if !approx_eq(roi_ratio, expected_ratio) {
        return Err(StateError::ValidationFailed(format!(
            "{label} roi_ratio must equal human_cost_usd / claude_cost_usd"
        )));
    }
    Ok(())
}

pub async fn build_roi_graph() -> Result<RoiGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("roi").await?;
    build_roi_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_roi_graph_with_ephemeral_sqlite() -> Result<RoiGraph, String> {
    build_roi_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_roi_graph_with_database_path(database_path: &str) -> Result<RoiGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_roi_graph_with_checkpointer(checkpointer).await
}

async fn build_roi_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<RoiGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = roi_state_schema();
    let builder = StateGraphBuilder::<RoiState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: RoiState| async move {
                emit_decision_node_event("roi", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_DATA,
            |s: RoiState| async move {
                emit_decision_node_event("roi", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = RoiDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_DATA, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            MISSING_ESTIMATE_DATA,
            |s: RoiState| async move {
                emit_decision_node_event("roi", MISSING_ESTIMATE_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = RoiDecision::MissingEstimateData;
                Ok::<_, NodeError>(next)
            },
            node_config(
                MISSING_ESTIMATE_DATA,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            POSITIVE_RETURN,
            |s: RoiState| async move {
                emit_decision_node_event("roi", POSITIVE_RETURN, &s.identifier)?;
                let mut next = s;
                next.decision = RoiDecision::PositiveReturn;
                Ok::<_, NodeError>(next)
            },
            node_config(POSITIVE_RETURN, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            BREAK_EVEN,
            |s: RoiState| async move {
                emit_decision_node_event("roi", BREAK_EVEN, &s.identifier)?;
                let mut next = s;
                next.decision = RoiDecision::BreakEven;
                Ok::<_, NodeError>(next)
            },
            node_config(BREAK_EVEN, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NEGATIVE_RETURN,
            |s: RoiState| async move {
                emit_decision_node_event("roi", NEGATIVE_RETURN, &s.identifier)?;
                let mut next = s;
                next.decision = RoiDecision::NegativeReturn;
                Ok::<_, NodeError>(next)
            },
            node_config(NEGATIVE_RETURN, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &RoiState| match expected_decision(s) {
            RoiDecision::NoData => NO_DATA.into(),
            RoiDecision::MissingEstimateData => MISSING_ESTIMATE_DATA.into(),
            RoiDecision::PositiveReturn => POSITIVE_RETURN.into(),
            RoiDecision::BreakEven => BREAK_EVEN.into(),
            RoiDecision::NegativeReturn => NEGATIVE_RETURN.into(),
            RoiDecision::Unclassified => NO_DATA.into(),
        })
        .add_edge(NO_DATA, END)
        .add_edge(MISSING_ESTIMATE_DATA, END)
        .add_edge(POSITIVE_RETURN, END)
        .add_edge(BREAK_EVEN, END)
        .add_edge(NEGATIVE_RETURN, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_roi_decision_report(
    compiled: &RoiGraph,
    state: RoiState,
) -> Result<RoiGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id("roi", "aggregate", &state)?;
    let streamed = stream_decision_run(compiled, &thread_id, "roi", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "roi",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(RoiGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: roi_graph_topology(compiled)?,
    })
}

pub fn roi_graph_topology(compiled: &RoiGraph) -> Result<DecisionGraphTopology, String> {
    topology("roi", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::roi::{human_baseline_per_point, HeadlineRoi, RoiReport, RoiWindow};

    fn roi_window(
        tickets_shipped: usize,
        points_shipped: f64,
        claude_cost_usd: f64,
        human_cost_usd: f64,
        estimate_data_available: bool,
        estimate_note: &str,
    ) -> RoiWindow {
        let roi_ratio = if claude_cost_usd > 0.0 {
            human_cost_usd / claude_cost_usd
        } else {
            0.0
        };
        RoiWindow {
            window_days: None,
            label: "all-time".to_string(),
            tickets_shipped,
            points_shipped,
            claude_cost_usd,
            claude_cost_per_point: if points_shipped > 0.0 {
                claude_cost_usd / points_shipped
            } else {
                0.0
            },
            human_cost_usd,
            roi_ratio,
            projected_annual_savings_usd: human_cost_usd - claude_cost_usd,
            estimate_data_available,
            estimate_note: estimate_note.to_string(),
        }
    }

    fn report(window: RoiWindow) -> RoiReport {
        let headline = HeadlineRoi {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            roi_ratio: window.roi_ratio,
            claude_cost_usd_total: window.claude_cost_usd,
            human_cost_usd_total: window.human_cost_usd,
            tickets_shipped_total: window.tickets_shipped,
            projected_annual_savings_usd: window.projected_annual_savings_usd,
            claude_cost_per_point: window.claude_cost_per_point,
            human_cost_per_point: human_baseline_per_point(),
            estimate_data_available: window.estimate_data_available,
            estimate_note: window.estimate_note.clone(),
            windows: vec![window.clone()],
        };
        RoiReport {
            windows: vec![window],
            headline: Some(headline),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_positive_roi_report() {
        let graph = build_roi_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let report = report(roi_window(2, 5.0, 10.0, 1_635.0, true, ""));
        let state = RoiState::from_report(&report);
        let run = run_roi_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, RoiDecision::PositiveReturn);
        assert_eq!(run.topology.graph, "roi");
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
            .roi_authorization()
            .expect("roi decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), RoiDecision::PositiveReturn);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_marks_missing_estimate_data() {
        let graph = build_roi_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let report = report(roi_window(
            2,
            0.0,
            10.0,
            0.0,
            false,
            "SEN-13 estimate data required; ROI not computed",
        ));
        let state = RoiState::from_report(&report);
        let run = run_roi_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, RoiDecision::MissingEstimateData);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_roi_math() {
        let graph = build_roi_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let report = report(roi_window(2, 5.0, 10.0, 1_635.0, true, ""));
        let mut state = RoiState::from_report(&report);
        state.roi_ratio = 99.0;

        let err = run_roi_decision_report(&graph, state)
            .await
            .expect_err("broken roi ratio should fail schema validation");
        assert!(err.contains("roi_ratio"));
    }
}
