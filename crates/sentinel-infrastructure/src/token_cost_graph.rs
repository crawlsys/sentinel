//! Graph-backed token cost report classification.
//!
//! The token-cost scanner prices cached vs uncached usage. This graph validates
//! the arithmetic and emits a checkpointed operational verdict for CLI and MCP.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::token_cost::TokenCostSummary;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TokenCostDecision {
    #[default]
    Unclassified,
    NoData,
    UnknownModelRisk,
    CacheEffective,
    NoSavings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCostState {
    pub identifier: String,
    pub tickets: usize,
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_with_caching_usd: f64,
    pub cost_without_caching_usd: f64,
    pub cache_savings_usd: f64,
    pub cache_savings_fraction: f64,
    pub unknown_model_tokens: u64,
    pub model_families: usize,
    pub decision: TokenCostDecision,
}

impl TokenCostState {
    #[must_use]
    pub fn from_summary(summary: &TokenCostSummary) -> Self {
        Self {
            identifier: "aggregate".to_string(),
            tickets: summary.tickets,
            total_tokens: summary.total_tokens,
            input_tokens: summary.input_tokens,
            output_tokens: summary.output_tokens,
            cache_write_tokens: summary.cache_write_tokens,
            cache_read_tokens: summary.cache_read_tokens,
            cost_with_caching_usd: summary.cost_with_caching_usd,
            cost_without_caching_usd: summary.cost_without_caching_usd,
            cache_savings_usd: summary.cache_savings_usd,
            cache_savings_fraction: summary.cache_savings_fraction,
            unknown_model_tokens: summary.unknown_model_tokens,
            model_families: summary.by_model.len(),
            decision: TokenCostDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenCostGraphRun {
    pub state: TokenCostState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<TokenCostState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct TokenCostAuthorization {
    decision: TokenCostDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl TokenCostAuthorization {
    #[must_use]
    pub fn decision(&self) -> TokenCostDecision {
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

impl TokenCostGraphRun {
    #[must_use]
    pub fn token_cost_authorization(&self) -> Result<Option<TokenCostAuthorization>, String> {
        if self.state.decision == TokenCostDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "token_cost",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(TokenCostAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const UNKNOWN_MODEL_RISK: &str = "unknown_model_risk";
const CACHE_EFFECTIVE: &str = "cache_effective";
const NO_SAVINGS: &str = "no_savings";

pub type TokenCostGraph = CompilationResult<TokenCostState>;

#[must_use]
pub fn token_cost_decision_label(decision: TokenCostDecision) -> &'static str {
    match decision {
        TokenCostDecision::Unclassified => "unclassified",
        TokenCostDecision::NoData => "no-data",
        TokenCostDecision::UnknownModelRisk => "unknown-model-risk",
        TokenCostDecision::CacheEffective => "cache-effective",
        TokenCostDecision::NoSavings => "no-savings",
    }
}

fn expected_decision(state: &TokenCostState) -> TokenCostDecision {
    if state.tickets == 0 || state.total_tokens == 0 {
        TokenCostDecision::NoData
    } else if state.unknown_model_tokens > 0 {
        TokenCostDecision::UnknownModelRisk
    } else if state.cache_savings_usd > 0.0 {
        TokenCostDecision::CacheEffective
    } else {
        TokenCostDecision::NoSavings
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "token_cost")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.000_001_f64.max(right.abs() * 0.000_001)
}

fn token_cost_state_schema() -> StateSchema<TokenCostState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tickets",
                "total_tokens",
                "input_tokens",
                "output_tokens",
                "cache_write_tokens",
                "cache_read_tokens",
                "cost_with_caching_usd",
                "cost_without_caching_usd",
                "cache_savings_usd",
                "cache_savings_fraction",
                "unknown_model_tokens",
                "model_families",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tickets": { "type": "integer", "minimum": 0 },
                "total_tokens": { "type": "integer", "minimum": 0 },
                "input_tokens": { "type": "integer", "minimum": 0 },
                "output_tokens": { "type": "integer", "minimum": 0 },
                "cache_write_tokens": { "type": "integer", "minimum": 0 },
                "cache_read_tokens": { "type": "integer", "minimum": 0 },
                "cost_with_caching_usd": { "type": "number", "minimum": 0 },
                "cost_without_caching_usd": { "type": "number", "minimum": 0 },
                "cache_savings_usd": { "type": "number" },
                "cache_savings_fraction": { "type": "number" },
                "unknown_model_tokens": { "type": "integer", "minimum": 0 },
                "model_families": { "type": "integer", "minimum": 0 },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "UnknownModelRisk",
                        "CacheEffective",
                        "NoSavings"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "token_cost",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &TokenCostState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "token_cost identifier must not be empty".to_string(),
                ));
            }
            let costs = [
                state.cost_with_caching_usd,
                state.cost_without_caching_usd,
                state.cache_savings_usd,
                state.cache_savings_fraction,
            ];
            if costs.iter().any(|cost| !cost.is_finite()) {
                return Err(StateError::ValidationFailed(
                    "token_cost monetary values must be finite".to_string(),
                ));
            }
            if state.cost_with_caching_usd < 0.0 || state.cost_without_caching_usd < 0.0 {
                return Err(StateError::ValidationFailed(
                    "token_cost costs must be non-negative".to_string(),
                ));
            }
            let expected_total = state.input_tokens
                + state.output_tokens
                + state.cache_write_tokens
                + state.cache_read_tokens;
            if state.total_tokens != expected_total {
                return Err(StateError::ValidationFailed(
                    "token_cost total_tokens must equal input+output+cache tokens".to_string(),
                ));
            }
            if state.unknown_model_tokens > state.total_tokens {
                return Err(StateError::ValidationFailed(
                    "token_cost unknown_model_tokens cannot exceed total_tokens".to_string(),
                ));
            }
            let expected_savings = state.cost_without_caching_usd - state.cost_with_caching_usd;
            if !approx_eq(state.cache_savings_usd, expected_savings) {
                return Err(StateError::ValidationFailed(
                    "token_cost cache_savings_usd must equal without-with cost".to_string(),
                ));
            }
            let expected_fraction = if state.cost_without_caching_usd > 0.0 {
                state.cache_savings_usd / state.cost_without_caching_usd
            } else {
                0.0
            };
            if !approx_eq(state.cache_savings_fraction, expected_fraction) {
                return Err(StateError::ValidationFailed(
                    "token_cost cache_savings_fraction must match savings/without".to_string(),
                ));
            }
            if state.decision != TokenCostDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "token_cost terminal decision must match token/cost inputs".to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_token_cost_graph() -> Result<TokenCostGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("token_cost").await?;
    build_token_cost_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_token_cost_graph_with_ephemeral_sqlite() -> Result<TokenCostGraph, String> {
    build_token_cost_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_token_cost_graph_with_database_path(
    database_path: &str,
) -> Result<TokenCostGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_token_cost_graph_with_checkpointer(checkpointer).await
}

async fn build_token_cost_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<TokenCostGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = token_cost_state_schema();
    let builder = StateGraphBuilder::<TokenCostState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: TokenCostState| async move {
                emit_decision_node_event("token_cost", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_DATA,
            |s: TokenCostState| async move {
                emit_decision_node_event("token_cost", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = TokenCostDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_DATA, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            UNKNOWN_MODEL_RISK,
            |s: TokenCostState| async move {
                emit_decision_node_event("token_cost", UNKNOWN_MODEL_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = TokenCostDecision::UnknownModelRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(UNKNOWN_MODEL_RISK, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            CACHE_EFFECTIVE,
            |s: TokenCostState| async move {
                emit_decision_node_event("token_cost", CACHE_EFFECTIVE, &s.identifier)?;
                let mut next = s;
                next.decision = TokenCostDecision::CacheEffective;
                Ok::<_, NodeError>(next)
            },
            node_config(CACHE_EFFECTIVE, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            NO_SAVINGS,
            |s: TokenCostState| async move {
                emit_decision_node_event("token_cost", NO_SAVINGS, &s.identifier)?;
                let mut next = s;
                next.decision = TokenCostDecision::NoSavings;
                Ok::<_, NodeError>(next)
            },
            node_config(NO_SAVINGS, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &TokenCostState| match expected_decision(s) {
            TokenCostDecision::NoData => NO_DATA.into(),
            TokenCostDecision::UnknownModelRisk => UNKNOWN_MODEL_RISK.into(),
            TokenCostDecision::CacheEffective => CACHE_EFFECTIVE.into(),
            TokenCostDecision::NoSavings => NO_SAVINGS.into(),
            TokenCostDecision::Unclassified => NO_DATA.into(),
        })
        .add_edge(NO_DATA, END)
        .add_edge(UNKNOWN_MODEL_RISK, END)
        .add_edge(CACHE_EFFECTIVE, END)
        .add_edge(NO_SAVINGS, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_token_cost_decision_report(
    compiled: &TokenCostGraph,
    state: TokenCostState,
) -> Result<TokenCostGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "token_cost",
        "aggregate",
        &state,
    )?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "token_cost", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "token_cost",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(TokenCostGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: token_cost_graph_topology(compiled)?,
    })
}

pub fn token_cost_graph_topology(
    compiled: &TokenCostGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("token_cost", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::token_cost::ModelCost;
    use std::collections::BTreeMap;

    fn summary(unknown_model_tokens: u64) -> TokenCostSummary {
        let mut by_model = BTreeMap::new();
        by_model.insert(
            if unknown_model_tokens > 0 {
                "unknown".to_string()
            } else {
                "opus".to_string()
            },
            ModelCost {
                tokens: 1_700_000,
                cached_usd: 4.25,
                uncached_usd: 8.00,
            },
        );
        TokenCostSummary {
            tickets: 2,
            total_tokens: 1_700_000,
            input_tokens: 500_000,
            output_tokens: 100_000,
            cache_write_tokens: 100_000,
            cache_read_tokens: 1_000_000,
            cost_with_caching_usd: 4.25,
            cost_without_caching_usd: 8.00,
            cache_savings_usd: 3.75,
            cache_savings_fraction: 3.75 / 8.00,
            unknown_model_tokens,
            by_model,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_cache_effective_cost_report() {
        let graph = build_token_cost_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = TokenCostState::from_summary(&summary(0));
        let run = run_token_cost_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, TokenCostDecision::CacheEffective);
        assert_eq!(run.topology.graph, "token_cost");
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
            .token_cost_authorization()
            .expect("token cost decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), TokenCostDecision::CacheEffective);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_prioritizes_unknown_model_risk() {
        let graph = build_token_cost_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = TokenCostState::from_summary(&summary(1_700_000));
        let run = run_token_cost_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, TokenCostDecision::UnknownModelRisk);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_cost_math() {
        let graph = build_token_cost_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = TokenCostState::from_summary(&summary(0));
        state.cache_savings_usd = 99.0;
        let err = run_token_cost_decision_report(&graph, state)
            .await
            .expect_err("broken savings should fail schema validation");
        assert!(err.contains("cache_savings_usd"));
    }
}
