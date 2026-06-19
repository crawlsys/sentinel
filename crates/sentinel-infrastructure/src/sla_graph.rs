//! Graph-backed SLA aggregate classification.
//!
//! The SLA aggregator rolls breach records into 24h, 7d, and 30d windows. This
//! graph validates the window math and emits a checkpointed operations verdict.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::sla::{BreachesSummary, SlaAggregate};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SlaDecision {
    #[default]
    Unclassified,
    NoData,
    NoWindowBreaches,
    RepeatedBreach,
    ActiveBreach,
    SaturatedBreachLoad,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaAggregateState {
    pub sla: String,
    pub breaches_24h: u64,
    pub breaches_7d: u64,
    pub breaches_30d: u64,
    pub most_recent: Option<String>,
}

impl SlaAggregateState {
    #[must_use]
    pub fn from_aggregate(aggregate: &SlaAggregate) -> Self {
        Self {
            sla: aggregate.sla.clone(),
            breaches_24h: aggregate.breaches_24h,
            breaches_7d: aggregate.breaches_7d,
            breaches_30d: aggregate.breaches_30d,
            most_recent: aggregate.most_recent.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaState {
    pub identifier: String,
    pub records_scanned: u64,
    pub slas_reported: usize,
    pub total_breaches_24h: u64,
    pub total_breaches_7d: u64,
    pub total_breaches_30d: u64,
    pub max_breaches_24h: u64,
    pub max_breaches_7d: u64,
    pub max_breaches_30d: u64,
    pub aggregates: Vec<SlaAggregateState>,
    pub decision: SlaDecision,
}

impl SlaState {
    #[must_use]
    pub fn from_summary(summary: &BreachesSummary) -> Self {
        let aggregates: Vec<SlaAggregateState> = summary
            .aggregates
            .iter()
            .map(SlaAggregateState::from_aggregate)
            .collect();
        let total_breaches_24h = aggregates.iter().map(|a| a.breaches_24h).sum();
        let total_breaches_7d = aggregates.iter().map(|a| a.breaches_7d).sum();
        let total_breaches_30d = aggregates.iter().map(|a| a.breaches_30d).sum();
        let max_breaches_24h = aggregates.iter().map(|a| a.breaches_24h).max().unwrap_or(0);
        let max_breaches_7d = aggregates.iter().map(|a| a.breaches_7d).max().unwrap_or(0);
        let max_breaches_30d = aggregates.iter().map(|a| a.breaches_30d).max().unwrap_or(0);
        Self {
            identifier: "aggregate".to_string(),
            records_scanned: summary.records_scanned,
            slas_reported: aggregates.len(),
            total_breaches_24h,
            total_breaches_7d,
            total_breaches_30d,
            max_breaches_24h,
            max_breaches_7d,
            max_breaches_30d,
            aggregates,
            decision: SlaDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SlaGraphRun {
    pub state: SlaState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SlaState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct SlaAuthorization {
    decision: SlaDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl SlaAuthorization {
    #[must_use]
    pub fn decision(&self) -> SlaDecision {
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

impl SlaGraphRun {
    #[must_use]
    pub fn sla_authorization(&self) -> Result<Option<SlaAuthorization>, String> {
        if self.state.decision == SlaDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "sla",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SlaAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const NO_WINDOW_BREACHES: &str = "no_window_breaches";
const REPEATED_BREACH: &str = "repeated_breach";
const ACTIVE_BREACH: &str = "active_breach";
const SATURATED_BREACH_LOAD: &str = "saturated_breach_load";

pub type SlaGraph = CompilationResult<SlaState>;

#[must_use]
pub fn sla_decision_label(decision: SlaDecision) -> &'static str {
    match decision {
        SlaDecision::Unclassified => "unclassified",
        SlaDecision::NoData => "no-data",
        SlaDecision::NoWindowBreaches => "no-window-breaches",
        SlaDecision::RepeatedBreach => "repeated-breach",
        SlaDecision::ActiveBreach => "active-breach",
        SlaDecision::SaturatedBreachLoad => "saturated-breach-load",
    }
}

fn expected_decision(state: &SlaState) -> SlaDecision {
    if state.records_scanned == 0 {
        SlaDecision::NoData
    } else if state.total_breaches_30d == 0 {
        SlaDecision::NoWindowBreaches
    } else if state.total_breaches_24h >= 5 || state.max_breaches_30d >= 10 {
        SlaDecision::SaturatedBreachLoad
    } else if state.total_breaches_7d > 0 {
        SlaDecision::ActiveBreach
    } else {
        SlaDecision::RepeatedBreach
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "sla")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn sla_state_schema() -> StateSchema<SlaState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "records_scanned",
                "slas_reported",
                "total_breaches_24h",
                "total_breaches_7d",
                "total_breaches_30d",
                "max_breaches_24h",
                "max_breaches_7d",
                "max_breaches_30d",
                "aggregates",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "records_scanned": { "type": "integer", "minimum": 0 },
                "slas_reported": { "type": "integer", "minimum": 0 },
                "total_breaches_24h": { "type": "integer", "minimum": 0 },
                "total_breaches_7d": { "type": "integer", "minimum": 0 },
                "total_breaches_30d": { "type": "integer", "minimum": 0 },
                "max_breaches_24h": { "type": "integer", "minimum": 0 },
                "max_breaches_7d": { "type": "integer", "minimum": 0 },
                "max_breaches_30d": { "type": "integer", "minimum": 0 },
                "aggregates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "sla",
                            "breaches_24h",
                            "breaches_7d",
                            "breaches_30d",
                            "most_recent"
                        ],
                        "properties": {
                            "sla": { "type": "string", "minLength": 1 },
                            "breaches_24h": { "type": "integer", "minimum": 0 },
                            "breaches_7d": { "type": "integer", "minimum": 0 },
                            "breaches_30d": { "type": "integer", "minimum": 0 },
                            "most_recent": {
                                "anyOf": [
                                    { "type": "string", "minLength": 1 },
                                    { "type": "null" }
                                ]
                            }
                        }
                    }
                },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "NoWindowBreaches",
                        "RepeatedBreach",
                        "ActiveBreach",
                        "SaturatedBreachLoad"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "sla",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SlaState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "sla identifier must not be empty".to_string(),
                ));
            }
            if state.slas_reported != state.aggregates.len() {
                return Err(StateError::ValidationFailed(
                    "sla slas_reported must equal aggregate count".to_string(),
                ));
            }
            let total_24h: u64 = state.aggregates.iter().map(|a| a.breaches_24h).sum();
            let total_7d: u64 = state.aggregates.iter().map(|a| a.breaches_7d).sum();
            let total_30d: u64 = state.aggregates.iter().map(|a| a.breaches_30d).sum();
            let max_24h = state
                .aggregates
                .iter()
                .map(|a| a.breaches_24h)
                .max()
                .unwrap_or(0);
            let max_7d = state
                .aggregates
                .iter()
                .map(|a| a.breaches_7d)
                .max()
                .unwrap_or(0);
            let max_30d = state
                .aggregates
                .iter()
                .map(|a| a.breaches_30d)
                .max()
                .unwrap_or(0);
            if total_24h != state.total_breaches_24h
                || total_7d != state.total_breaches_7d
                || total_30d != state.total_breaches_30d
                || max_24h != state.max_breaches_24h
                || max_7d != state.max_breaches_7d
                || max_30d != state.max_breaches_30d
            {
                return Err(StateError::ValidationFailed(
                    "sla aggregate totals must match aggregate rows".to_string(),
                ));
            }
            if state.total_breaches_24h > state.total_breaches_7d
                || state.total_breaches_7d > state.total_breaches_30d
                || state.total_breaches_30d > state.records_scanned
            {
                return Err(StateError::ValidationFailed(
                    "sla aggregate totals must respect 24h <= 7d <= 30d <= records".to_string(),
                ));
            }
            for aggregate in &state.aggregates {
                if aggregate.sla.trim().is_empty() {
                    return Err(StateError::ValidationFailed(
                        "sla aggregate name must not be empty".to_string(),
                    ));
                }
                if aggregate.breaches_24h > aggregate.breaches_7d
                    || aggregate.breaches_7d > aggregate.breaches_30d
                {
                    return Err(StateError::ValidationFailed(
                        "sla aggregate windows must respect 24h <= 7d <= 30d".to_string(),
                    ));
                }
            }
            if state.decision != SlaDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "sla terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_sla_graph() -> Result<SlaGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("sla").await?;
    build_sla_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_sla_graph_with_ephemeral_sqlite() -> Result<SlaGraph, String> {
    build_sla_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_sla_graph_with_database_path(database_path: &str) -> Result<SlaGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_sla_graph_with_checkpointer(checkpointer).await
}

async fn build_sla_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SlaGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = sla_state_schema();
    let builder = StateGraphBuilder::<SlaState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: SlaState| async move {
                emit_decision_node_event("sla", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_DATA,
            |s: SlaState| async move {
                emit_decision_node_event("sla", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = SlaDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_DATA, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_WINDOW_BREACHES,
            |s: SlaState| async move {
                emit_decision_node_event("sla", NO_WINDOW_BREACHES, &s.identifier)?;
                let mut next = s;
                next.decision = SlaDecision::NoWindowBreaches;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_WINDOW_BREACHES, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            REPEATED_BREACH,
            |s: SlaState| async move {
                emit_decision_node_event("sla", REPEATED_BREACH, &s.identifier)?;
                let mut next = s;
                next.decision = SlaDecision::RepeatedBreach;
                Ok::<_, NodeError>(next)
            },
            node_config(REPEATED_BREACH, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ACTIVE_BREACH,
            |s: SlaState| async move {
                emit_decision_node_event("sla", ACTIVE_BREACH, &s.identifier)?;
                let mut next = s;
                next.decision = SlaDecision::ActiveBreach;
                Ok::<_, NodeError>(next)
            },
            node_config(ACTIVE_BREACH, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            SATURATED_BREACH_LOAD,
            |s: SlaState| async move {
                emit_decision_node_event("sla", SATURATED_BREACH_LOAD, &s.identifier)?;
                let mut next = s;
                next.decision = SlaDecision::SaturatedBreachLoad;
                Ok::<_, NodeError>(next)
            },
            node_config(
                SATURATED_BREACH_LOAD,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &SlaState| match expected_decision(s) {
            SlaDecision::NoData => NO_DATA.into(),
            SlaDecision::NoWindowBreaches => NO_WINDOW_BREACHES.into(),
            SlaDecision::RepeatedBreach => REPEATED_BREACH.into(),
            SlaDecision::ActiveBreach => ACTIVE_BREACH.into(),
            SlaDecision::SaturatedBreachLoad => SATURATED_BREACH_LOAD.into(),
            SlaDecision::Unclassified => NO_DATA.into(),
        })
        .add_edge(NO_DATA, END)
        .add_edge(NO_WINDOW_BREACHES, END)
        .add_edge(REPEATED_BREACH, END)
        .add_edge(ACTIVE_BREACH, END)
        .add_edge(SATURATED_BREACH_LOAD, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_sla_decision_report(
    compiled: &SlaGraph,
    state: SlaState,
) -> Result<SlaGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "sla",
        "aggregate",
        &state,
    )?;
    let streamed = stream_decision_run(compiled, &thread_id, "sla", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "sla",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SlaGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: sla_graph_topology(compiled)?,
    })
}

pub fn sla_graph_topology(compiled: &SlaGraph) -> Result<DecisionGraphTopology, String> {
    topology("sla", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> BreachesSummary {
        BreachesSummary {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            records_scanned: 3,
            aggregates: vec![SlaAggregate {
                sla: "P0 pickup".to_string(),
                breaches_24h: 1,
                breaches_7d: 2,
                breaches_30d: 3,
                most_recent: Some("2026-06-18T00:00:00Z".to_string()),
            }],
        }
    }

    #[tokio::test]
    async fn graph_authorizes_active_breach_summary() {
        let graph = build_sla_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = SlaState::from_summary(&summary());
        let run = run_sla_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, SlaDecision::ActiveBreach);
        assert_eq!(run.topology.graph, "sla");
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
            .sla_authorization()
            .expect("sla decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), SlaDecision::ActiveBreach);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_marks_no_data() {
        let graph = build_sla_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let empty = BreachesSummary {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            records_scanned: 0,
            aggregates: Vec::new(),
        };
        let state = SlaState::from_summary(&empty);
        let run = run_sla_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, SlaDecision::NoData);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_window_math() {
        let graph = build_sla_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = SlaState::from_summary(&summary());
        state.total_breaches_7d = 99;

        let err = run_sla_decision_report(&graph, state)
            .await
            .expect_err("broken SLA totals should fail schema validation");
        assert!(err.contains("totals"));
    }
}
