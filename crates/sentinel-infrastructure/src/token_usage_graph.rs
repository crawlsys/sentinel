//! Graph-backed token usage scan classification.
//!
//! The SEN-7 scanner attributes Claude Code sessions to Linear tickets. This
//! graph validates attribution totals and emits a checkpointed scan verdict.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::tokens::ScanReport;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TokenUsageDecision {
    #[default]
    Unclassified,
    NoData,
    NoMappedTickets,
    UnpricedModelRisk,
    MappingCoverageRisk,
    ExpensiveTicketRisk,
    HealthyUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageTopTicketState {
    pub ticket: String,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageState {
    pub identifier: String,
    pub total_sessions: u64,
    pub mapped_sessions: u64,
    pub unmapped_sessions: u64,
    pub unpriced_sessions: u64,
    pub unpriced_tokens: u64,
    pub tickets: u64,
    pub mapping_coverage_fraction: f64,
    pub top_ticket_count: usize,
    pub highest_ticket_cost_usd: f64,
    pub top_n_expensive: Vec<TokenUsageTopTicketState>,
    pub decision: TokenUsageDecision,
}

impl TokenUsageState {
    #[must_use]
    pub fn from_report(report: &ScanReport) -> Self {
        let top_n_expensive: Vec<TokenUsageTopTicketState> = report
            .top_n_expensive
            .iter()
            .map(|(ticket, cost_usd)| TokenUsageTopTicketState {
                ticket: ticket.clone(),
                cost_usd: *cost_usd,
            })
            .collect();
        let highest_ticket_cost_usd = top_n_expensive
            .first()
            .map_or(0.0, |ticket| ticket.cost_usd);
        let mapping_coverage_fraction = if report.total_sessions == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let mapped = report.mapped_sessions as f64;
            #[allow(clippy::cast_precision_loss)]
            let total = report.total_sessions as f64;
            mapped / total
        };
        Self {
            identifier: "aggregate".to_string(),
            total_sessions: report.total_sessions,
            mapped_sessions: report.mapped_sessions,
            unmapped_sessions: report.unmapped_sessions,
            unpriced_sessions: report.unpriced_sessions,
            unpriced_tokens: report.unpriced_tokens,
            tickets: report.tickets,
            mapping_coverage_fraction,
            top_ticket_count: top_n_expensive.len(),
            highest_ticket_cost_usd,
            top_n_expensive,
            decision: TokenUsageDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsageGraphRun {
    pub state: TokenUsageState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<TokenUsageState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct TokenUsageAuthorization {
    decision: TokenUsageDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl TokenUsageAuthorization {
    #[must_use]
    pub fn decision(&self) -> TokenUsageDecision {
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

impl TokenUsageGraphRun {
    #[must_use]
    pub fn token_usage_authorization(&self) -> Result<Option<TokenUsageAuthorization>, String> {
        if self.state.decision == TokenUsageDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "token_usage",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(TokenUsageAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const NO_MAPPED_TICKETS: &str = "no_mapped_tickets";
const UNPRICED_MODEL_RISK: &str = "unpriced_model_risk";
const MAPPING_COVERAGE_RISK: &str = "mapping_coverage_risk";
const EXPENSIVE_TICKET_RISK: &str = "expensive_ticket_risk";
const HEALTHY_USAGE: &str = "healthy_usage";

pub type TokenUsageGraph = CompilationResult<TokenUsageState>;

#[must_use]
pub fn token_usage_decision_label(decision: TokenUsageDecision) -> &'static str {
    match decision {
        TokenUsageDecision::Unclassified => "unclassified",
        TokenUsageDecision::NoData => "no-data",
        TokenUsageDecision::NoMappedTickets => "no-mapped-tickets",
        TokenUsageDecision::UnpricedModelRisk => "unpriced-model-risk",
        TokenUsageDecision::MappingCoverageRisk => "mapping-coverage-risk",
        TokenUsageDecision::ExpensiveTicketRisk => "expensive-ticket-risk",
        TokenUsageDecision::HealthyUsage => "healthy-usage",
    }
}

fn expected_decision(state: &TokenUsageState) -> TokenUsageDecision {
    if state.total_sessions == 0 {
        TokenUsageDecision::NoData
    } else if state.mapped_sessions == 0 {
        TokenUsageDecision::NoMappedTickets
    } else if state.unpriced_tokens > 0 {
        TokenUsageDecision::UnpricedModelRisk
    } else if state.mapping_coverage_fraction < 0.80 {
        TokenUsageDecision::MappingCoverageRisk
    } else if state.highest_ticket_cost_usd >= 100.0 {
        TokenUsageDecision::ExpensiveTicketRisk
    } else {
        TokenUsageDecision::HealthyUsage
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "token_usage")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.000_001_f64.max(right.abs() * 0.000_001)
}

fn token_usage_state_schema() -> StateSchema<TokenUsageState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "total_sessions",
                "mapped_sessions",
                "unmapped_sessions",
                "unpriced_sessions",
                "unpriced_tokens",
                "tickets",
                "mapping_coverage_fraction",
                "top_ticket_count",
                "highest_ticket_cost_usd",
                "top_n_expensive",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "total_sessions": { "type": "integer", "minimum": 0 },
                "mapped_sessions": { "type": "integer", "minimum": 0 },
                "unmapped_sessions": { "type": "integer", "minimum": 0 },
                "unpriced_sessions": { "type": "integer", "minimum": 0 },
                "unpriced_tokens": { "type": "integer", "minimum": 0 },
                "tickets": { "type": "integer", "minimum": 0 },
                "mapping_coverage_fraction": { "type": "number", "minimum": 0, "maximum": 1 },
                "top_ticket_count": { "type": "integer", "minimum": 0 },
                "highest_ticket_cost_usd": { "type": "number", "minimum": 0 },
                "top_n_expensive": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["ticket", "cost_usd"],
                        "properties": {
                            "ticket": { "type": "string", "minLength": 1 },
                            "cost_usd": { "type": "number", "minimum": 0 }
                        }
                    }
                },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "NoMappedTickets",
                        "UnpricedModelRisk",
                        "MappingCoverageRisk",
                        "ExpensiveTicketRisk",
                        "HealthyUsage"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "token_usage",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &TokenUsageState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "token_usage identifier must not be empty".to_string(),
                ));
            }
            if state.mapped_sessions + state.unmapped_sessions != state.total_sessions {
                return Err(StateError::ValidationFailed(
                    "token_usage mapped + unmapped sessions must equal total_sessions".to_string(),
                ));
            }
            if state.unpriced_sessions > state.mapped_sessions {
                return Err(StateError::ValidationFailed(
                    "token_usage unpriced_sessions cannot exceed mapped_sessions".to_string(),
                ));
            }
            if state.unpriced_tokens > 0 && state.unpriced_sessions == 0 {
                return Err(StateError::ValidationFailed(
                    "token_usage unpriced_tokens require unpriced_sessions".to_string(),
                ));
            }
            if state.tickets > state.mapped_sessions {
                return Err(StateError::ValidationFailed(
                    "token_usage tickets cannot exceed mapped_sessions".to_string(),
                ));
            }
            #[allow(clippy::cast_precision_loss)]
            let expected_coverage = if state.total_sessions == 0 {
                0.0
            } else {
                state.mapped_sessions as f64 / state.total_sessions as f64
            };
            if !state.mapping_coverage_fraction.is_finite()
                || !approx_eq(state.mapping_coverage_fraction, expected_coverage)
            {
                return Err(StateError::ValidationFailed(
                    "token_usage mapping coverage must match session counts".to_string(),
                ));
            }
            if state.top_ticket_count != state.top_n_expensive.len() {
                return Err(StateError::ValidationFailed(
                    "token_usage top_ticket_count must match top_n rows".to_string(),
                ));
            }
            if state.top_ticket_count > 10 || state.top_ticket_count as u64 > state.tickets {
                return Err(StateError::ValidationFailed(
                    "token_usage top tickets must fit the output contract".to_string(),
                ));
            }
            validate_top_tickets(state)?;
            if state.decision != TokenUsageDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "token_usage terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

fn validate_top_tickets(state: &TokenUsageState) -> Result<(), StateError> {
    let expected_highest = state
        .top_n_expensive
        .first()
        .map_or(0.0, |ticket| ticket.cost_usd);
    if !state.highest_ticket_cost_usd.is_finite()
        || state.highest_ticket_cost_usd < 0.0
        || !approx_eq(state.highest_ticket_cost_usd, expected_highest)
    {
        return Err(StateError::ValidationFailed(
            "token_usage highest ticket cost must match the first top row".to_string(),
        ));
    }
    let mut previous = f64::INFINITY;
    for ticket in &state.top_n_expensive {
        if ticket.ticket.trim().is_empty() {
            return Err(StateError::ValidationFailed(
                "token_usage top ticket id must not be empty".to_string(),
            ));
        }
        if !ticket.cost_usd.is_finite() || ticket.cost_usd < 0.0 {
            return Err(StateError::ValidationFailed(
                "token_usage top ticket costs must be finite and non-negative".to_string(),
            ));
        }
        if ticket.cost_usd > previous {
            return Err(StateError::ValidationFailed(
                "token_usage top tickets must be sorted by descending cost".to_string(),
            ));
        }
        previous = ticket.cost_usd;
    }
    Ok(())
}

pub async fn build_token_usage_graph() -> Result<TokenUsageGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("token_usage").await?;
    build_token_usage_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_token_usage_graph_with_ephemeral_sqlite() -> Result<TokenUsageGraph, String> {
    build_token_usage_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_token_usage_graph_with_database_path(
    database_path: &str,
) -> Result<TokenUsageGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_token_usage_graph_with_checkpointer(checkpointer).await
}

async fn build_token_usage_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<TokenUsageGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = token_usage_state_schema();
    let builder = StateGraphBuilder::<TokenUsageState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            NO_DATA,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = TokenUsageDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NO_DATA,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            NO_MAPPED_TICKETS,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", NO_MAPPED_TICKETS, &s.identifier)?;
                let mut next = s;
                next.decision = TokenUsageDecision::NoMappedTickets;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NO_MAPPED_TICKETS,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            UNPRICED_MODEL_RISK,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", UNPRICED_MODEL_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = TokenUsageDecision::UnpricedModelRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                UNPRICED_MODEL_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            MAPPING_COVERAGE_RISK,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", MAPPING_COVERAGE_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = TokenUsageDecision::MappingCoverageRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                MAPPING_COVERAGE_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            EXPENSIVE_TICKET_RISK,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", EXPENSIVE_TICKET_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = TokenUsageDecision::ExpensiveTicketRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                EXPENSIVE_TICKET_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            HEALTHY_USAGE,
            |s: TokenUsageState| async move {
                emit_decision_node_event("token_usage", HEALTHY_USAGE, &s.identifier)?;
                let mut next = s;
                next.decision = TokenUsageDecision::HealthyUsage;
                Ok::<_, NodeError>(next)
            },
            node_config(
                HEALTHY_USAGE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &TokenUsageState| match expected_decision(s) {
            TokenUsageDecision::NoData => NO_DATA.into(),
            TokenUsageDecision::NoMappedTickets => NO_MAPPED_TICKETS.into(),
            TokenUsageDecision::UnpricedModelRisk => UNPRICED_MODEL_RISK.into(),
            TokenUsageDecision::MappingCoverageRisk => MAPPING_COVERAGE_RISK.into(),
            TokenUsageDecision::ExpensiveTicketRisk => EXPENSIVE_TICKET_RISK.into(),
            TokenUsageDecision::HealthyUsage => HEALTHY_USAGE.into(),
            TokenUsageDecision::Unclassified => NO_DATA.into(),
        })
        .add_edge(NO_DATA, END)
        .add_edge(NO_MAPPED_TICKETS, END)
        .add_edge(UNPRICED_MODEL_RISK, END)
        .add_edge(MAPPING_COVERAGE_RISK, END)
        .add_edge(EXPENSIVE_TICKET_RISK, END)
        .add_edge(HEALTHY_USAGE, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_token_usage_decision_report(
    compiled: &TokenUsageGraph,
    state: TokenUsageState,
) -> Result<TokenUsageGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "token_usage",
        "aggregate",
        &state,
    )?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "token_usage", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "token_usage",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(TokenUsageGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: token_usage_graph_topology(compiled)?,
    })
}

pub fn token_usage_graph_topology(
    compiled: &TokenUsageGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("token_usage", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> ScanReport {
        ScanReport {
            total_sessions: 10,
            mapped_sessions: 9,
            unmapped_sessions: 1,
            unpriced_sessions: 0,
            unpriced_tokens: 0,
            tickets: 2,
            top_n_expensive: vec![("SEN-1".to_string(), 42.0), ("SEN-2".to_string(), 10.0)],
        }
    }

    #[tokio::test]
    async fn graph_authorizes_healthy_usage_report() {
        let graph = build_token_usage_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = TokenUsageState::from_report(&report());
        let run = run_token_usage_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, TokenUsageDecision::HealthyUsage);
        assert_eq!(run.topology.graph, "token_usage");
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
            .token_usage_authorization()
            .expect("token usage decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), TokenUsageDecision::HealthyUsage);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let graph = build_token_usage_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = TokenUsageState::from_report(&report());
        let mut run = run_token_usage_decision_report(&graph, state)
            .await
            .expect("graph runs");
        run.write_history.clear();

        let err = run
            .token_usage_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_prioritizes_mapping_coverage_risk() {
        let graph = build_token_usage_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut report = report();
        report.mapped_sessions = 7;
        report.unmapped_sessions = 3;
        let state = TokenUsageState::from_report(&report);
        let run = run_token_usage_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, TokenUsageDecision::MappingCoverageRisk);
    }

    #[tokio::test]
    async fn graph_authorizes_unpriced_model_risk() {
        let graph = build_token_usage_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut report = report();
        report.unpriced_sessions = 1;
        report.unpriced_tokens = 1_000_000;
        let state = TokenUsageState::from_report(&report);
        let run = run_token_usage_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, TokenUsageDecision::UnpricedModelRisk);
    }

    #[tokio::test]
    async fn graph_schema_rejects_unsorted_top_tickets() {
        let graph = build_token_usage_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = TokenUsageState::from_report(&report());
        state.top_n_expensive.swap(0, 1);
        state.highest_ticket_cost_usd = state.top_n_expensive[0].cost_usd;

        let err = run_token_usage_decision_report(&graph, state)
            .await
            .expect_err("unsorted top tickets should fail schema validation");
        assert!(err.contains("descending cost"));
    }
}
