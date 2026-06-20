//! Graph-backed deploy frequency classification.
//!
//! The deploy-frequency aggregator computes DORA deploy cadence. This graph
//! validates the rate and tier math before the CLI presents the summary.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::deploy_freq::{DeploySummary, DoraTier, RepoEnvAggregate};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeployFrequencyDecision {
    #[default]
    Unclassified,
    NoData,
    NoActiveDeployWindow,
    LowFrequencyRisk,
    NeedsImprovement,
    HealthyCadence,
    EliteCadence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployFrequencyAggregateState {
    pub repo: String,
    pub env: String,
    pub deploys_7d: u64,
    pub deploys_30d: u64,
    pub rate_per_day_7d: f64,
    pub rate_per_day_30d: f64,
    pub tier: String,
}

impl DeployFrequencyAggregateState {
    #[must_use]
    pub fn from_aggregate(aggregate: &RepoEnvAggregate) -> Self {
        Self {
            repo: aggregate.repo.clone(),
            env: aggregate.env.clone(),
            deploys_7d: aggregate.deploys_7d,
            deploys_30d: aggregate.deploys_30d,
            rate_per_day_7d: aggregate.rate_per_day_7d,
            rate_per_day_30d: aggregate.rate_per_day_30d,
            tier: dora_tier_label(aggregate.tier).to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployFrequencyState {
    pub identifier: String,
    pub records_scanned: u64,
    pub repo_env_pairs: usize,
    pub daily_points: usize,
    pub total_deploys_7d: u64,
    pub total_deploys_30d: u64,
    pub total_deploys_60d: u64,
    pub elite_pairs: usize,
    pub high_pairs: usize,
    pub medium_pairs: usize,
    pub low_pairs: usize,
    pub aggregates: Vec<DeployFrequencyAggregateState>,
    pub decision: DeployFrequencyDecision,
}

impl DeployFrequencyState {
    #[must_use]
    pub fn from_summary(summary: &DeploySummary) -> Self {
        let aggregates: Vec<DeployFrequencyAggregateState> = summary
            .aggregates
            .iter()
            .map(DeployFrequencyAggregateState::from_aggregate)
            .collect();
        let total_deploys_7d = aggregates.iter().map(|a| a.deploys_7d).sum();
        let total_deploys_30d = aggregates.iter().map(|a| a.deploys_30d).sum();
        let total_deploys_60d = summary.daily_points.iter().map(|point| point.deploys).sum();
        let elite_pairs = aggregates
            .iter()
            .filter(|a| a.tier == dora_tier_label(DoraTier::Elite))
            .count();
        let high_pairs = aggregates
            .iter()
            .filter(|a| a.tier == dora_tier_label(DoraTier::High))
            .count();
        let medium_pairs = aggregates
            .iter()
            .filter(|a| a.tier == dora_tier_label(DoraTier::Medium))
            .count();
        let low_pairs = aggregates
            .iter()
            .filter(|a| a.tier == dora_tier_label(DoraTier::Low))
            .count();
        Self {
            identifier: "aggregate".to_string(),
            records_scanned: summary.records_scanned,
            repo_env_pairs: aggregates.len(),
            daily_points: summary.daily_points.len(),
            total_deploys_7d,
            total_deploys_30d,
            total_deploys_60d,
            elite_pairs,
            high_pairs,
            medium_pairs,
            low_pairs,
            aggregates,
            decision: DeployFrequencyDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeployFrequencyGraphRun {
    pub state: DeployFrequencyState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DeployFrequencyState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct DeployFrequencyAuthorization {
    decision: DeployFrequencyDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl DeployFrequencyAuthorization {
    #[must_use]
    pub fn decision(&self) -> DeployFrequencyDecision {
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

impl DeployFrequencyGraphRun {
    #[must_use]
    pub fn deploy_frequency_authorization(
        &self,
    ) -> Result<Option<DeployFrequencyAuthorization>, String> {
        if self.state.decision == DeployFrequencyDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "deploy_frequency",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(DeployFrequencyAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const NO_ACTIVE_DEPLOY_WINDOW: &str = "no_active_deploy_window";
const LOW_FREQUENCY_RISK: &str = "low_frequency_risk";
const NEEDS_IMPROVEMENT: &str = "needs_improvement";
const HEALTHY_CADENCE: &str = "healthy_cadence";
const ELITE_CADENCE: &str = "elite_cadence";

pub type DeployFrequencyGraph = CompilationResult<DeployFrequencyState>;

#[must_use]
pub fn deploy_frequency_decision_label(decision: DeployFrequencyDecision) -> &'static str {
    match decision {
        DeployFrequencyDecision::Unclassified => "unclassified",
        DeployFrequencyDecision::NoData => "no-data",
        DeployFrequencyDecision::NoActiveDeployWindow => "no-active-deploy-window",
        DeployFrequencyDecision::LowFrequencyRisk => "low-frequency-risk",
        DeployFrequencyDecision::NeedsImprovement => "needs-improvement",
        DeployFrequencyDecision::HealthyCadence => "healthy-cadence",
        DeployFrequencyDecision::EliteCadence => "elite-cadence",
    }
}

fn dora_tier_label(tier: DoraTier) -> &'static str {
    match tier {
        DoraTier::Elite => "elite",
        DoraTier::High => "high",
        DoraTier::Medium => "medium",
        DoraTier::Low => "low",
    }
}

fn expected_decision(state: &DeployFrequencyState) -> DeployFrequencyDecision {
    if state.records_scanned == 0 {
        DeployFrequencyDecision::NoData
    } else if state.repo_env_pairs == 0 || state.total_deploys_30d == 0 {
        DeployFrequencyDecision::NoActiveDeployWindow
    } else if state.low_pairs > 0 {
        DeployFrequencyDecision::LowFrequencyRisk
    } else if state.medium_pairs > 0 {
        DeployFrequencyDecision::NeedsImprovement
    } else if state.elite_pairs == state.repo_env_pairs {
        DeployFrequencyDecision::EliteCadence
    } else {
        DeployFrequencyDecision::HealthyCadence
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "deploy_frequency")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn approx_eq(left: f64, right: f64) -> bool {
    (left - right).abs() <= 0.000_001_f64.max(right.abs() * 0.000_001)
}

fn deploy_frequency_state_schema() -> StateSchema<DeployFrequencyState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "records_scanned",
                "repo_env_pairs",
                "daily_points",
                "total_deploys_7d",
                "total_deploys_30d",
                "total_deploys_60d",
                "elite_pairs",
                "high_pairs",
                "medium_pairs",
                "low_pairs",
                "aggregates",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "records_scanned": { "type": "integer", "minimum": 0 },
                "repo_env_pairs": { "type": "integer", "minimum": 0 },
                "daily_points": { "type": "integer", "minimum": 0 },
                "total_deploys_7d": { "type": "integer", "minimum": 0 },
                "total_deploys_30d": { "type": "integer", "minimum": 0 },
                "total_deploys_60d": { "type": "integer", "minimum": 0 },
                "elite_pairs": { "type": "integer", "minimum": 0 },
                "high_pairs": { "type": "integer", "minimum": 0 },
                "medium_pairs": { "type": "integer", "minimum": 0 },
                "low_pairs": { "type": "integer", "minimum": 0 },
                "aggregates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "repo",
                            "env",
                            "deploys_7d",
                            "deploys_30d",
                            "rate_per_day_7d",
                            "rate_per_day_30d",
                            "tier"
                        ],
                        "properties": {
                            "repo": { "type": "string", "minLength": 1 },
                            "env": { "type": "string", "minLength": 1 },
                            "deploys_7d": { "type": "integer", "minimum": 0 },
                            "deploys_30d": { "type": "integer", "minimum": 0 },
                            "rate_per_day_7d": { "type": "number", "minimum": 0 },
                            "rate_per_day_30d": { "type": "number", "minimum": 0 },
                            "tier": {
                                "type": "string",
                                "enum": ["elite", "high", "medium", "low"]
                            }
                        }
                    }
                },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "NoActiveDeployWindow",
                        "LowFrequencyRisk",
                        "NeedsImprovement",
                        "HealthyCadence",
                        "EliteCadence"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "deploy_frequency",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &DeployFrequencyState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "deploy_frequency identifier must not be empty".to_string(),
                ));
            }
            if state.repo_env_pairs != state.aggregates.len() {
                return Err(StateError::ValidationFailed(
                    "deploy_frequency repo_env_pairs must equal aggregate count".to_string(),
                ));
            }
            let total_7d: u64 = state.aggregates.iter().map(|a| a.deploys_7d).sum();
            let total_30d: u64 = state.aggregates.iter().map(|a| a.deploys_30d).sum();
            if total_7d != state.total_deploys_7d || total_30d != state.total_deploys_30d {
                return Err(StateError::ValidationFailed(
                    "deploy_frequency totals must match aggregate rows".to_string(),
                ));
            }
            if state.total_deploys_7d > state.total_deploys_30d
                || state.total_deploys_30d > state.total_deploys_60d
                || state.total_deploys_60d > state.records_scanned
            {
                return Err(StateError::ValidationFailed(
                    "deploy_frequency totals must respect 7d <= 30d <= 60d <= records".to_string(),
                ));
            }
            let elite = state
                .aggregates
                .iter()
                .filter(|a| a.tier == dora_tier_label(DoraTier::Elite))
                .count();
            let high = state
                .aggregates
                .iter()
                .filter(|a| a.tier == dora_tier_label(DoraTier::High))
                .count();
            let medium = state
                .aggregates
                .iter()
                .filter(|a| a.tier == dora_tier_label(DoraTier::Medium))
                .count();
            let low = state
                .aggregates
                .iter()
                .filter(|a| a.tier == dora_tier_label(DoraTier::Low))
                .count();
            if elite != state.elite_pairs
                || high != state.high_pairs
                || medium != state.medium_pairs
                || low != state.low_pairs
                || elite + high + medium + low != state.repo_env_pairs
            {
                return Err(StateError::ValidationFailed(
                    "deploy_frequency tier counts must match aggregate rows".to_string(),
                ));
            }
            for aggregate in &state.aggregates {
                validate_aggregate(aggregate)?;
            }
            if state.decision != DeployFrequencyDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "deploy_frequency terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

fn validate_aggregate(aggregate: &DeployFrequencyAggregateState) -> Result<(), StateError> {
    if aggregate.repo.trim().is_empty() || aggregate.env.trim().is_empty() {
        return Err(StateError::ValidationFailed(
            "deploy_frequency repo and env must not be empty".to_string(),
        ));
    }
    if aggregate.deploys_7d > aggregate.deploys_30d {
        return Err(StateError::ValidationFailed(
            "deploy_frequency aggregate deploys_7d cannot exceed deploys_30d".to_string(),
        ));
    }
    if !aggregate.rate_per_day_7d.is_finite()
        || !aggregate.rate_per_day_30d.is_finite()
        || aggregate.rate_per_day_7d < 0.0
        || aggregate.rate_per_day_30d < 0.0
    {
        return Err(StateError::ValidationFailed(
            "deploy_frequency aggregate rates must be finite and non-negative".to_string(),
        ));
    }
    #[allow(clippy::cast_precision_loss)]
    let expected_7d = aggregate.deploys_7d as f64 / 7.0;
    #[allow(clippy::cast_precision_loss)]
    let expected_30d = aggregate.deploys_30d as f64 / 30.0;
    if !approx_eq(aggregate.rate_per_day_7d, expected_7d)
        || !approx_eq(aggregate.rate_per_day_30d, expected_30d)
    {
        return Err(StateError::ValidationFailed(
            "deploy_frequency rates must match deploy counts".to_string(),
        ));
    }
    let expected_tier = dora_tier_label(DoraTier::from_rate_per_day(aggregate.rate_per_day_30d));
    if aggregate.tier != expected_tier {
        return Err(StateError::ValidationFailed(
            "deploy_frequency tier must match 30d deploy rate".to_string(),
        ));
    }
    Ok(())
}

pub async fn build_deploy_frequency_graph() -> Result<DeployFrequencyGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("deploy_frequency").await?;
    build_deploy_frequency_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_deploy_frequency_graph_with_ephemeral_sqlite() -> Result<DeployFrequencyGraph, String>
{
    build_deploy_frequency_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_deploy_frequency_graph_with_database_path(
    database_path: &str,
) -> Result<DeployFrequencyGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_deploy_frequency_graph_with_checkpointer(checkpointer).await
}

async fn build_deploy_frequency_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<DeployFrequencyGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = deploy_frequency_state_schema();
    let builder = StateGraphBuilder::<DeployFrequencyState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event("deploy_frequency", CLASSIFY, &s.identifier)?;
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
            NO_DATA,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event("deploy_frequency", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = DeployFrequencyDecision::NoData;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NO_DATA,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            NO_ACTIVE_DEPLOY_WINDOW,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event(
                    "deploy_frequency",
                    NO_ACTIVE_DEPLOY_WINDOW,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = DeployFrequencyDecision::NoActiveDeployWindow;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NO_ACTIVE_DEPLOY_WINDOW,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            LOW_FREQUENCY_RISK,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event("deploy_frequency", LOW_FREQUENCY_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = DeployFrequencyDecision::LowFrequencyRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                LOW_FREQUENCY_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            NEEDS_IMPROVEMENT,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event("deploy_frequency", NEEDS_IMPROVEMENT, &s.identifier)?;
                let mut next = s;
                next.decision = DeployFrequencyDecision::NeedsImprovement;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NEEDS_IMPROVEMENT,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            HEALTHY_CADENCE,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event("deploy_frequency", HEALTHY_CADENCE, &s.identifier)?;
                let mut next = s;
                next.decision = DeployFrequencyDecision::HealthyCadence;
                Ok::<_, NodeError>(next)
            },
            node_config(
                HEALTHY_CADENCE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ELITE_CADENCE,
            |s: DeployFrequencyState| async move {
                emit_decision_node_event("deploy_frequency", ELITE_CADENCE, &s.identifier)?;
                let mut next = s;
                next.decision = DeployFrequencyDecision::EliteCadence;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ELITE_CADENCE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &DeployFrequencyState| match expected_decision(s) {
                DeployFrequencyDecision::NoData => NO_DATA.into(),
                DeployFrequencyDecision::NoActiveDeployWindow => NO_ACTIVE_DEPLOY_WINDOW.into(),
                DeployFrequencyDecision::LowFrequencyRisk => LOW_FREQUENCY_RISK.into(),
                DeployFrequencyDecision::NeedsImprovement => NEEDS_IMPROVEMENT.into(),
                DeployFrequencyDecision::HealthyCadence => HEALTHY_CADENCE.into(),
                DeployFrequencyDecision::EliteCadence => ELITE_CADENCE.into(),
                DeployFrequencyDecision::Unclassified => NO_DATA.into(),
            },
        )
        .add_edge(NO_DATA, END)
        .add_edge(NO_ACTIVE_DEPLOY_WINDOW, END)
        .add_edge(LOW_FREQUENCY_RISK, END)
        .add_edge(NEEDS_IMPROVEMENT, END)
        .add_edge(HEALTHY_CADENCE, END)
        .add_edge(ELITE_CADENCE, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_deploy_frequency_decision_report(
    compiled: &DeployFrequencyGraph,
    state: DeployFrequencyState,
) -> Result<DeployFrequencyGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "deploy_frequency",
        "aggregate",
        &state,
    )?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "deploy_frequency", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "deploy_frequency",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(DeployFrequencyGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: deploy_frequency_graph_topology(compiled)?,
    })
}

pub fn deploy_frequency_graph_topology(
    compiled: &DeployFrequencyGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("deploy_frequency", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::deploy_freq::{DailyPoint, DeploySummary, RepoEnvAggregate};

    fn aggregate(
        repo: &str,
        env: &str,
        deploys_7d: u64,
        deploys_30d: u64,
        tier: DoraTier,
    ) -> RepoEnvAggregate {
        RepoEnvAggregate {
            repo: repo.to_string(),
            env: env.to_string(),
            deploys_7d,
            deploys_30d,
            rate_per_day_7d: deploys_7d as f64 / 7.0,
            rate_per_day_30d: deploys_30d as f64 / 30.0,
            tier,
            first_in_window: Some("2026-06-01T00:00:00Z".to_string()),
            last_in_window: Some("2026-06-18T00:00:00Z".to_string()),
        }
    }

    fn summary() -> DeploySummary {
        DeploySummary {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            records_scanned: 30,
            aggregates: vec![aggregate("sentinel", "prod", 7, 30, DoraTier::Elite)],
            daily_points: vec![DailyPoint {
                date: "2026-06-18".to_string(),
                repo: "sentinel".to_string(),
                env: "prod".to_string(),
                deploys: 30,
            }],
        }
    }

    #[tokio::test]
    async fn graph_authorizes_elite_deploy_frequency() {
        let graph = build_deploy_frequency_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = DeployFrequencyState::from_summary(&summary());
        let run = run_deploy_frequency_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, DeployFrequencyDecision::EliteCadence);
        assert_eq!(run.topology.graph, "deploy_frequency");
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
            .deploy_frequency_authorization()
            .expect("deploy frequency decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(
            authorization.decision(),
            DeployFrequencyDecision::EliteCadence
        );
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_prioritizes_low_frequency_risk() {
        let graph = build_deploy_frequency_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let low = DeploySummary {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            records_scanned: 1,
            aggregates: vec![aggregate("sentinel", "prod", 0, 1, DoraTier::Medium)],
            daily_points: vec![DailyPoint {
                date: "2026-06-18".to_string(),
                repo: "sentinel".to_string(),
                env: "prod".to_string(),
                deploys: 1,
            }],
        };
        let state = DeployFrequencyState::from_summary(&low);
        let run = run_deploy_frequency_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(
            run.state.decision,
            DeployFrequencyDecision::NeedsImprovement
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_rate_math() {
        let graph = build_deploy_frequency_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = DeployFrequencyState::from_summary(&summary());
        state.aggregates[0].rate_per_day_30d = 99.0;

        let err = run_deploy_frequency_decision_report(&graph, state)
            .await
            .expect_err("broken rate should fail schema validation");
        assert!(err.contains("rates"));
    }
}
