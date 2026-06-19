//! Graph-backed cache efficiency classification.
//!
//! The SEN-14 scanner summarizes prompt-cache hit rates. This graph validates
//! the aggregate ranges and emits a checkpointed cache-efficiency verdict.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::cache_efficiency::{CacheReport, DailyPoint, WorstSession};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CacheEfficiencyDecision {
    #[default]
    Unclassified,
    NoData,
    NoUsageData,
    CacheWasteRisk,
    NeedsTuning,
    CacheEffective,
    CacheExcellent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheWorstSessionState {
    pub session_id: String,
    pub project: String,
    pub date: String,
    pub hit_rate: f64,
    pub total_input_tokens: u64,
    pub waste_estimate_usd: f64,
}

impl CacheWorstSessionState {
    #[must_use]
    pub fn from_worst_session(session: &WorstSession) -> Self {
        Self {
            session_id: session.session_id.clone(),
            project: session.project.clone(),
            date: session.date.clone(),
            hit_rate: session.hit_rate,
            total_input_tokens: session.total_input_tokens,
            waste_estimate_usd: session.waste_estimate_usd,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheDailyPointState {
    pub date: String,
    pub sessions: u64,
    pub hit_rate: f64,
}

impl CacheDailyPointState {
    #[must_use]
    pub fn from_daily_point(point: &DailyPoint) -> Self {
        Self {
            date: point.date.clone(),
            sessions: point.sessions,
            hit_rate: point.hit_rate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEfficiencyState {
    pub identifier: String,
    pub sessions_scanned: u64,
    pub sessions_with_usage: u64,
    pub usage_coverage_fraction: f64,
    pub p50_hit_rate: f64,
    pub p90_hit_rate: f64,
    pub worst_sessions_count: usize,
    pub daily_points_count: usize,
    pub max_waste_estimate_usd: f64,
    pub min_worst_hit_rate: f64,
    pub worst_sessions: Vec<CacheWorstSessionState>,
    pub daily_trend: Vec<CacheDailyPointState>,
    pub decision: CacheEfficiencyDecision,
}

impl CacheEfficiencyState {
    #[must_use]
    pub fn from_report(report: &CacheReport) -> Self {
        let worst_sessions: Vec<CacheWorstSessionState> = report
            .worst_sessions
            .iter()
            .map(CacheWorstSessionState::from_worst_session)
            .collect();
        let daily_trend: Vec<CacheDailyPointState> = report
            .daily_trend
            .iter()
            .map(CacheDailyPointState::from_daily_point)
            .collect();
        let usage_coverage_fraction = if report.sessions_scanned == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let with_usage = report.sessions_with_usage as f64;
            #[allow(clippy::cast_precision_loss)]
            let scanned = report.sessions_scanned as f64;
            with_usage / scanned
        };
        let max_waste_estimate_usd = worst_sessions
            .iter()
            .map(|session| session.waste_estimate_usd)
            .reduce(f64::max)
            .unwrap_or(0.0);
        let min_worst_hit_rate = worst_sessions
            .iter()
            .map(|session| session.hit_rate)
            .reduce(f64::min)
            .unwrap_or(1.0);
        Self {
            identifier: "aggregate".to_string(),
            sessions_scanned: report.sessions_scanned,
            sessions_with_usage: report.sessions_with_usage,
            usage_coverage_fraction,
            p50_hit_rate: report.p50_hit_rate,
            p90_hit_rate: report.p90_hit_rate,
            worst_sessions_count: worst_sessions.len(),
            daily_points_count: daily_trend.len(),
            max_waste_estimate_usd,
            min_worst_hit_rate,
            worst_sessions,
            daily_trend,
            decision: CacheEfficiencyDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheEfficiencyGraphRun {
    pub state: CacheEfficiencyState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<CacheEfficiencyState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct CacheEfficiencyAuthorization {
    decision: CacheEfficiencyDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl CacheEfficiencyAuthorization {
    #[must_use]
    pub fn decision(&self) -> CacheEfficiencyDecision {
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

impl CacheEfficiencyGraphRun {
    #[must_use]
    pub fn cache_efficiency_authorization(
        &self,
    ) -> Result<Option<CacheEfficiencyAuthorization>, String> {
        if self.state.decision == CacheEfficiencyDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "cache_efficiency",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(CacheEfficiencyAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const NO_USAGE_DATA: &str = "no_usage_data";
const CACHE_WASTE_RISK: &str = "cache_waste_risk";
const NEEDS_TUNING: &str = "needs_tuning";
const CACHE_EFFECTIVE: &str = "cache_effective";
const CACHE_EXCELLENT: &str = "cache_excellent";

pub type CacheEfficiencyGraph = CompilationResult<CacheEfficiencyState>;

#[must_use]
pub fn cache_efficiency_decision_label(decision: CacheEfficiencyDecision) -> &'static str {
    match decision {
        CacheEfficiencyDecision::Unclassified => "unclassified",
        CacheEfficiencyDecision::NoData => "no-data",
        CacheEfficiencyDecision::NoUsageData => "no-usage-data",
        CacheEfficiencyDecision::CacheWasteRisk => "cache-waste-risk",
        CacheEfficiencyDecision::NeedsTuning => "needs-tuning",
        CacheEfficiencyDecision::CacheEffective => "cache-effective",
        CacheEfficiencyDecision::CacheExcellent => "cache-excellent",
    }
}

fn expected_decision(state: &CacheEfficiencyState) -> CacheEfficiencyDecision {
    if state.sessions_scanned == 0 {
        CacheEfficiencyDecision::NoData
    } else if state.sessions_with_usage == 0 {
        CacheEfficiencyDecision::NoUsageData
    } else if state.p50_hit_rate < 0.50 || state.min_worst_hit_rate < 0.20 {
        CacheEfficiencyDecision::CacheWasteRisk
    } else if state.p50_hit_rate < 0.75 || state.p90_hit_rate < 0.85 {
        CacheEfficiencyDecision::NeedsTuning
    } else if state.p50_hit_rate >= 0.90 {
        CacheEfficiencyDecision::CacheExcellent
    } else {
        CacheEfficiencyDecision::CacheEffective
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "cache_efficiency")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.000_001_f64.max(right.abs() * 0.000_001)
}

fn cache_efficiency_state_schema() -> StateSchema<CacheEfficiencyState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "sessions_scanned",
                "sessions_with_usage",
                "usage_coverage_fraction",
                "p50_hit_rate",
                "p90_hit_rate",
                "worst_sessions_count",
                "daily_points_count",
                "max_waste_estimate_usd",
                "min_worst_hit_rate",
                "worst_sessions",
                "daily_trend",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "sessions_scanned": { "type": "integer", "minimum": 0 },
                "sessions_with_usage": { "type": "integer", "minimum": 0 },
                "usage_coverage_fraction": { "type": "number", "minimum": 0, "maximum": 1 },
                "p50_hit_rate": { "type": "number", "minimum": 0, "maximum": 1 },
                "p90_hit_rate": { "type": "number", "minimum": 0, "maximum": 1 },
                "worst_sessions_count": { "type": "integer", "minimum": 0 },
                "daily_points_count": { "type": "integer", "minimum": 0 },
                "max_waste_estimate_usd": { "type": "number", "minimum": 0 },
                "min_worst_hit_rate": { "type": "number", "minimum": 0, "maximum": 1 },
                "worst_sessions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "session_id",
                            "project",
                            "date",
                            "hit_rate",
                            "total_input_tokens",
                            "waste_estimate_usd"
                        ],
                        "properties": {
                            "session_id": { "type": "string", "minLength": 1 },
                            "project": { "type": "string", "minLength": 1 },
                            "date": { "type": "string", "minLength": 1 },
                            "hit_rate": { "type": "number", "minimum": 0, "maximum": 1 },
                            "total_input_tokens": { "type": "integer", "minimum": 0 },
                            "waste_estimate_usd": { "type": "number", "minimum": 0 }
                        }
                    }
                },
                "daily_trend": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["date", "sessions", "hit_rate"],
                        "properties": {
                            "date": { "type": "string", "minLength": 1 },
                            "sessions": { "type": "integer", "minimum": 1 },
                            "hit_rate": { "type": "number", "minimum": 0, "maximum": 1 }
                        }
                    }
                },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "NoUsageData",
                        "CacheWasteRisk",
                        "NeedsTuning",
                        "CacheEffective",
                        "CacheExcellent"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "cache_efficiency",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &CacheEfficiencyState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency identifier must not be empty".to_string(),
                ));
            }
            if state.sessions_with_usage > state.sessions_scanned {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency sessions_with_usage cannot exceed sessions_scanned"
                        .to_string(),
                ));
            }
            validate_rate("cache_efficiency p50", state.p50_hit_rate)?;
            validate_rate("cache_efficiency p90", state.p90_hit_rate)?;
            if state.p50_hit_rate > state.p90_hit_rate {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency p50_hit_rate cannot exceed p90_hit_rate".to_string(),
                ));
            }
            #[allow(clippy::cast_precision_loss)]
            let expected_coverage = if state.sessions_scanned == 0 {
                0.0
            } else {
                state.sessions_with_usage as f64 / state.sessions_scanned as f64
            };
            if !state.usage_coverage_fraction.is_finite()
                || !approx_eq(state.usage_coverage_fraction, expected_coverage)
            {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency usage coverage must match session counts".to_string(),
                ));
            }
            if state.worst_sessions_count != state.worst_sessions.len() {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency worst_sessions_count must match row count".to_string(),
                ));
            }
            if state.daily_points_count != state.daily_trend.len() {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency daily_points_count must match row count".to_string(),
                ));
            }
            validate_worst_sessions(state)?;
            validate_daily_trend(state)?;
            if state.sessions_with_usage == 0
                && (state.p50_hit_rate > 0.0
                    || state.p90_hit_rate > 0.0
                    || !state.worst_sessions.is_empty()
                    || !state.daily_trend.is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency no-usage state must not carry rate evidence".to_string(),
                ));
            }
            if state.decision != CacheEfficiencyDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "cache_efficiency terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

fn validate_rate(label: &str, value: f64) -> Result<(), StateError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(StateError::ValidationFailed(format!(
            "{label} must be finite and between 0 and 1"
        )));
    }
    Ok(())
}

fn validate_worst_sessions(state: &CacheEfficiencyState) -> Result<(), StateError> {
    for session in &state.worst_sessions {
        if session.session_id.trim().is_empty() || session.project.trim().is_empty() {
            return Err(StateError::ValidationFailed(
                "cache_efficiency worst session identifiers must not be empty".to_string(),
            ));
        }
        validate_rate("cache_efficiency worst session hit_rate", session.hit_rate)?;
        if !session.waste_estimate_usd.is_finite() || session.waste_estimate_usd < 0.0 {
            return Err(StateError::ValidationFailed(
                "cache_efficiency waste estimates must be finite and non-negative".to_string(),
            ));
        }
    }
    let expected_max_waste = state
        .worst_sessions
        .iter()
        .map(|session| session.waste_estimate_usd)
        .reduce(f64::max)
        .unwrap_or(0.0);
    let expected_min_hit_rate = state
        .worst_sessions
        .iter()
        .map(|session| session.hit_rate)
        .reduce(f64::min)
        .unwrap_or(1.0);
    if !state.max_waste_estimate_usd.is_finite()
        || state.max_waste_estimate_usd < 0.0
        || !approx_eq(state.max_waste_estimate_usd, expected_max_waste)
        || !approx_eq(state.min_worst_hit_rate, expected_min_hit_rate)
    {
        return Err(StateError::ValidationFailed(
            "cache_efficiency worst-session extrema must match rows".to_string(),
        ));
    }
    Ok(())
}

fn validate_daily_trend(state: &CacheEfficiencyState) -> Result<(), StateError> {
    for point in &state.daily_trend {
        if point.date.trim().is_empty() {
            return Err(StateError::ValidationFailed(
                "cache_efficiency daily date must not be empty".to_string(),
            ));
        }
        if point.sessions == 0 {
            return Err(StateError::ValidationFailed(
                "cache_efficiency daily point must have at least one session".to_string(),
            ));
        }
        validate_rate("cache_efficiency daily hit_rate", point.hit_rate)?;
    }
    Ok(())
}

pub async fn build_cache_efficiency_graph() -> Result<CacheEfficiencyGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("cache_efficiency").await?;
    build_cache_efficiency_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_cache_efficiency_graph_with_ephemeral_sqlite() -> Result<CacheEfficiencyGraph, String>
{
    build_cache_efficiency_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_cache_efficiency_graph_with_database_path(
    database_path: &str,
) -> Result<CacheEfficiencyGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_cache_efficiency_graph_with_checkpointer(checkpointer).await
}

async fn build_cache_efficiency_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<CacheEfficiencyGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = cache_efficiency_state_schema();
    let builder = StateGraphBuilder::<CacheEfficiencyState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_DATA,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = CacheEfficiencyDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_DATA, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_USAGE_DATA,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", NO_USAGE_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = CacheEfficiencyDecision::NoUsageData;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_USAGE_DATA, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            CACHE_WASTE_RISK,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", CACHE_WASTE_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = CacheEfficiencyDecision::CacheWasteRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(CACHE_WASTE_RISK, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NEEDS_TUNING,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", NEEDS_TUNING, &s.identifier)?;
                let mut next = s;
                next.decision = CacheEfficiencyDecision::NeedsTuning;
                Ok::<_, NodeError>(next)
            },
            node_config(NEEDS_TUNING, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            CACHE_EFFECTIVE,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", CACHE_EFFECTIVE, &s.identifier)?;
                let mut next = s;
                next.decision = CacheEfficiencyDecision::CacheEffective;
                Ok::<_, NodeError>(next)
            },
            node_config(CACHE_EFFECTIVE, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            CACHE_EXCELLENT,
            |s: CacheEfficiencyState| async move {
                emit_decision_node_event("cache_efficiency", CACHE_EXCELLENT, &s.identifier)?;
                let mut next = s;
                next.decision = CacheEfficiencyDecision::CacheExcellent;
                Ok::<_, NodeError>(next)
            },
            node_config(CACHE_EXCELLENT, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &CacheEfficiencyState| match expected_decision(s) {
                CacheEfficiencyDecision::NoData => NO_DATA.into(),
                CacheEfficiencyDecision::NoUsageData => NO_USAGE_DATA.into(),
                CacheEfficiencyDecision::CacheWasteRisk => CACHE_WASTE_RISK.into(),
                CacheEfficiencyDecision::NeedsTuning => NEEDS_TUNING.into(),
                CacheEfficiencyDecision::CacheEffective => CACHE_EFFECTIVE.into(),
                CacheEfficiencyDecision::CacheExcellent => CACHE_EXCELLENT.into(),
                CacheEfficiencyDecision::Unclassified => NO_DATA.into(),
            },
        )
        .add_edge(NO_DATA, END)
        .add_edge(NO_USAGE_DATA, END)
        .add_edge(CACHE_WASTE_RISK, END)
        .add_edge(NEEDS_TUNING, END)
        .add_edge(CACHE_EFFECTIVE, END)
        .add_edge(CACHE_EXCELLENT, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_cache_efficiency_decision_report(
    compiled: &CacheEfficiencyGraph,
    state: CacheEfficiencyState,
) -> Result<CacheEfficiencyGraphRun, String> {
    let thread_id =
        crate::decision_graph_store::run_thread_id("cache_efficiency", "aggregate", &state)?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "cache_efficiency", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "cache_efficiency",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(CacheEfficiencyGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: cache_efficiency_graph_topology(compiled)?,
    })
}

pub fn cache_efficiency_graph_topology(
    compiled: &CacheEfficiencyGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("cache_efficiency", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::cache_efficiency::{CacheReport, DailyPoint, WorstSession};

    fn report(p50_hit_rate: f64, p90_hit_rate: f64, worst_hit_rate: f64) -> CacheReport {
        CacheReport {
            sessions_scanned: 4,
            sessions_with_usage: 4,
            p50_hit_rate,
            p90_hit_rate,
            worst_sessions: vec![WorstSession {
                session_id: "session-1".to_string(),
                project: "sentinel".to_string(),
                date: "2026-06-18".to_string(),
                hit_rate: worst_hit_rate,
                total_input_tokens: 100_000,
                waste_estimate_usd: 1.50,
            }],
            daily_trend: vec![DailyPoint {
                date: "2026-06-18".to_string(),
                sessions: 4,
                hit_rate: p50_hit_rate,
            }],
        }
    }

    #[tokio::test]
    async fn graph_authorizes_excellent_cache_report() {
        let graph = build_cache_efficiency_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = CacheEfficiencyState::from_report(&report(0.92, 0.98, 0.88));
        let run = run_cache_efficiency_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, CacheEfficiencyDecision::CacheExcellent);
        assert_eq!(run.topology.graph, "cache_efficiency");
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
            .cache_efficiency_authorization()
            .expect("cache efficiency decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(
            authorization.decision(),
            CacheEfficiencyDecision::CacheExcellent
        );
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_prioritizes_cache_waste_risk() {
        let graph = build_cache_efficiency_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = CacheEfficiencyState::from_report(&report(0.82, 0.95, 0.10));
        let run = run_cache_efficiency_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, CacheEfficiencyDecision::CacheWasteRisk);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_percentile_ordering() {
        let graph = build_cache_efficiency_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = CacheEfficiencyState::from_report(&report(0.90, 0.50, 0.80));

        let err = run_cache_efficiency_decision_report(&graph, state)
            .await
            .expect_err("broken percentile ordering should fail schema validation");
        assert!(err.contains("p50_hit_rate"));
    }
}
