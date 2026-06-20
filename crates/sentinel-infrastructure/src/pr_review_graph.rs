//! Graph-backed PR review report classification.
//!
//! The SEN-18 scanner summarizes review depth, latency, bot findings, and
//! human-in-the-loop coverage. This graph validates the aggregate report and
//! emits a checkpointed review-loop verdict.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::pr_review::{PerRepo, PrReviewReport};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PrReviewDecision {
    #[default]
    Unclassified,
    NoData,
    HumanReviewRisk,
    ReviewLatencyRisk,
    FindingLoadRisk,
    HealthyReviewLoop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrReviewRepoState {
    pub repo: String,
    pub prs: u64,
    pub avg_comments: f64,
    pub p50_ttfr_hours: f64,
    pub p90_ttfr_hours: f64,
    pub codex_findings: u64,
    pub coderabbit_findings: u64,
    pub human_review_pct: f64,
}

impl PrReviewRepoState {
    #[must_use]
    pub fn from_repo(repo: &PerRepo) -> Self {
        Self {
            repo: repo.repo.clone(),
            prs: repo.prs,
            avg_comments: repo.avg_comments,
            p50_ttfr_hours: repo.p50_ttfr_hours,
            p90_ttfr_hours: repo.p90_ttfr_hours,
            codex_findings: repo.codex_findings,
            coderabbit_findings: repo.coderabbit_findings,
            human_review_pct: repo.human_review_pct,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrReviewState {
    pub identifier: String,
    pub window_days: u32,
    pub repos_requested: usize,
    pub repos_reported: usize,
    pub total_prs: u64,
    pub avg_comments_per_pr: f64,
    pub p50_time_to_first_review_hours: f64,
    pub p90_time_to_first_review_hours: f64,
    pub codex_findings_total: u64,
    pub coderabbit_findings_total: u64,
    pub total_findings: u64,
    pub findings_per_pr: f64,
    pub human_review_pct: f64,
    pub per_repo: Vec<PrReviewRepoState>,
    pub decision: PrReviewDecision,
}

impl PrReviewState {
    #[must_use]
    pub fn from_report(report: &PrReviewReport) -> Self {
        let per_repo: Vec<PrReviewRepoState> = report
            .per_repo
            .iter()
            .map(PrReviewRepoState::from_repo)
            .collect();
        let total_findings = report.codex_findings_total + report.coderabbit_findings_total;
        let findings_per_pr = if report.total_prs == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let findings = total_findings as f64;
            #[allow(clippy::cast_precision_loss)]
            let prs = report.total_prs as f64;
            findings / prs
        };
        Self {
            identifier: "aggregate".to_string(),
            window_days: report.window_days,
            repos_requested: report.repos.len(),
            repos_reported: per_repo.len(),
            total_prs: report.total_prs,
            avg_comments_per_pr: report.avg_comments_per_pr,
            p50_time_to_first_review_hours: report.p50_time_to_first_review_hours,
            p90_time_to_first_review_hours: report.p90_time_to_first_review_hours,
            codex_findings_total: report.codex_findings_total,
            coderabbit_findings_total: report.coderabbit_findings_total,
            total_findings,
            findings_per_pr,
            human_review_pct: report.human_review_pct,
            per_repo,
            decision: PrReviewDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PrReviewGraphRun {
    pub state: PrReviewState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<PrReviewState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct PrReviewAuthorization {
    decision: PrReviewDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl PrReviewAuthorization {
    #[must_use]
    pub fn decision(&self) -> PrReviewDecision {
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

impl PrReviewGraphRun {
    #[must_use]
    pub fn pr_review_authorization(&self) -> Result<Option<PrReviewAuthorization>, String> {
        if self.state.decision == PrReviewDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "pr_review",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(PrReviewAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const NO_DATA: &str = "no_data";
const HUMAN_REVIEW_RISK: &str = "human_review_risk";
const REVIEW_LATENCY_RISK: &str = "review_latency_risk";
const FINDING_LOAD_RISK: &str = "finding_load_risk";
const HEALTHY_REVIEW_LOOP: &str = "healthy_review_loop";

pub type PrReviewGraph = CompilationResult<PrReviewState>;

#[must_use]
pub fn pr_review_decision_label(decision: PrReviewDecision) -> &'static str {
    match decision {
        PrReviewDecision::Unclassified => "unclassified",
        PrReviewDecision::NoData => "no-data",
        PrReviewDecision::HumanReviewRisk => "human-review-risk",
        PrReviewDecision::ReviewLatencyRisk => "review-latency-risk",
        PrReviewDecision::FindingLoadRisk => "finding-load-risk",
        PrReviewDecision::HealthyReviewLoop => "healthy-review-loop",
    }
}

fn expected_decision(state: &PrReviewState) -> PrReviewDecision {
    if state.total_prs == 0 {
        PrReviewDecision::NoData
    } else if state.human_review_pct < 50.0 {
        PrReviewDecision::HumanReviewRisk
    } else if state.p90_time_to_first_review_hours > 48.0 {
        PrReviewDecision::ReviewLatencyRisk
    } else if state.findings_per_pr > 3.0 {
        PrReviewDecision::FindingLoadRisk
    } else {
        PrReviewDecision::HealthyReviewLoop
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "pr_review")
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
    (left - right).abs() <= 0.01_f64.max(right.abs() * 0.000_001)
}

fn pr_review_state_schema() -> StateSchema<PrReviewState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "window_days",
                "repos_requested",
                "repos_reported",
                "total_prs",
                "avg_comments_per_pr",
                "p50_time_to_first_review_hours",
                "p90_time_to_first_review_hours",
                "codex_findings_total",
                "coderabbit_findings_total",
                "total_findings",
                "findings_per_pr",
                "human_review_pct",
                "per_repo",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "window_days": { "type": "integer", "minimum": 0 },
                "repos_requested": { "type": "integer", "minimum": 0 },
                "repos_reported": { "type": "integer", "minimum": 0 },
                "total_prs": { "type": "integer", "minimum": 0 },
                "avg_comments_per_pr": { "type": "number", "minimum": 0 },
                "p50_time_to_first_review_hours": { "type": "number", "minimum": 0 },
                "p90_time_to_first_review_hours": { "type": "number", "minimum": 0 },
                "codex_findings_total": { "type": "integer", "minimum": 0 },
                "coderabbit_findings_total": { "type": "integer", "minimum": 0 },
                "total_findings": { "type": "integer", "minimum": 0 },
                "findings_per_pr": { "type": "number", "minimum": 0 },
                "human_review_pct": { "type": "number", "minimum": 0, "maximum": 100 },
                "per_repo": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": [
                            "repo",
                            "prs",
                            "avg_comments",
                            "p50_ttfr_hours",
                            "p90_ttfr_hours",
                            "codex_findings",
                            "coderabbit_findings",
                            "human_review_pct"
                        ],
                        "properties": {
                            "repo": { "type": "string", "minLength": 1 },
                            "prs": { "type": "integer", "minimum": 0 },
                            "avg_comments": { "type": "number", "minimum": 0 },
                            "p50_ttfr_hours": { "type": "number", "minimum": 0 },
                            "p90_ttfr_hours": { "type": "number", "minimum": 0 },
                            "codex_findings": { "type": "integer", "minimum": 0 },
                            "coderabbit_findings": { "type": "integer", "minimum": 0 },
                            "human_review_pct": { "type": "number", "minimum": 0, "maximum": 100 }
                        }
                    }
                },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "NoData",
                        "HumanReviewRisk",
                        "ReviewLatencyRisk",
                        "FindingLoadRisk",
                        "HealthyReviewLoop"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "pr_review",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &PrReviewState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "pr_review identifier must not be empty".to_string(),
                ));
            }
            if state.repos_reported != state.per_repo.len() {
                return Err(StateError::ValidationFailed(
                    "pr_review repos_reported must equal per-repo rows".to_string(),
                ));
            }
            let total_prs: u64 = state.per_repo.iter().map(|repo| repo.prs).sum();
            let codex_total: u64 = state.per_repo.iter().map(|repo| repo.codex_findings).sum();
            let coderabbit_total: u64 = state
                .per_repo
                .iter()
                .map(|repo| repo.coderabbit_findings)
                .sum();
            if total_prs != state.total_prs
                || codex_total != state.codex_findings_total
                || coderabbit_total != state.coderabbit_findings_total
            {
                return Err(StateError::ValidationFailed(
                    "pr_review totals must match per-repo rows".to_string(),
                ));
            }
            if state.total_findings != state.codex_findings_total + state.coderabbit_findings_total
            {
                return Err(StateError::ValidationFailed(
                    "pr_review total_findings must equal Codex plus CodeRabbit findings"
                        .to_string(),
                ));
            }
            validate_nonnegative_finite("pr_review avg comments", state.avg_comments_per_pr)?;
            validate_nonnegative_finite(
                "pr_review p50 time-to-first-review",
                state.p50_time_to_first_review_hours,
            )?;
            validate_nonnegative_finite(
                "pr_review p90 time-to-first-review",
                state.p90_time_to_first_review_hours,
            )?;
            validate_pct("pr_review human_review_pct", state.human_review_pct)?;
            if state.p50_time_to_first_review_hours > state.p90_time_to_first_review_hours {
                return Err(StateError::ValidationFailed(
                    "pr_review p50 time-to-first-review cannot exceed p90".to_string(),
                ));
            }
            #[allow(clippy::cast_precision_loss)]
            let expected_findings_per_pr = if state.total_prs == 0 {
                0.0
            } else {
                state.total_findings as f64 / state.total_prs as f64
            };
            if !state.findings_per_pr.is_finite()
                || !approx_eq(state.findings_per_pr, expected_findings_per_pr)
            {
                return Err(StateError::ValidationFailed(
                    "pr_review findings_per_pr must match totals".to_string(),
                ));
            }
            for repo in &state.per_repo {
                validate_repo(repo)?;
            }
            if state.total_prs == 0
                && (state.avg_comments_per_pr > 0.0
                    || state.p50_time_to_first_review_hours > 0.0
                    || state.p90_time_to_first_review_hours > 0.0
                    || state.human_review_pct > 0.0
                    || state.total_findings > 0)
            {
                return Err(StateError::ValidationFailed(
                    "pr_review no-data state must not carry review metrics".to_string(),
                ));
            }
            if state.decision != PrReviewDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "pr_review terminal decision must match aggregate inputs".to_string(),
                ));
            }
            Ok(())
        })
}

fn validate_repo(repo: &PrReviewRepoState) -> Result<(), StateError> {
    if repo.repo.trim().is_empty() {
        return Err(StateError::ValidationFailed(
            "pr_review repo name must not be empty".to_string(),
        ));
    }
    validate_nonnegative_finite("pr_review repo avg_comments", repo.avg_comments)?;
    validate_nonnegative_finite("pr_review repo p50_ttfr_hours", repo.p50_ttfr_hours)?;
    validate_nonnegative_finite("pr_review repo p90_ttfr_hours", repo.p90_ttfr_hours)?;
    validate_pct("pr_review repo human_review_pct", repo.human_review_pct)?;
    if repo.p50_ttfr_hours > repo.p90_ttfr_hours {
        return Err(StateError::ValidationFailed(
            "pr_review repo p50_ttfr_hours cannot exceed p90".to_string(),
        ));
    }
    if repo.prs == 0
        && (repo.avg_comments > 0.0
            || repo.p50_ttfr_hours > 0.0
            || repo.p90_ttfr_hours > 0.0
            || repo.human_review_pct > 0.0
            || repo.codex_findings > 0
            || repo.coderabbit_findings > 0)
    {
        return Err(StateError::ValidationFailed(
            "pr_review empty repo row must not carry review metrics".to_string(),
        ));
    }
    Ok(())
}

fn validate_nonnegative_finite(label: &str, value: f64) -> Result<(), StateError> {
    if !value.is_finite() || value < 0.0 {
        return Err(StateError::ValidationFailed(format!(
            "{label} must be finite and non-negative"
        )));
    }
    Ok(())
}

fn validate_pct(label: &str, value: f64) -> Result<(), StateError> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(StateError::ValidationFailed(format!(
            "{label} must be between 0 and 100"
        )));
    }
    Ok(())
}

pub async fn build_pr_review_graph() -> Result<PrReviewGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("pr_review").await?;
    build_pr_review_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_pr_review_graph_with_ephemeral_sqlite() -> Result<PrReviewGraph, String> {
    build_pr_review_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_pr_review_graph_with_database_path(
    database_path: &str,
) -> Result<PrReviewGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_pr_review_graph_with_checkpointer(checkpointer).await
}

async fn build_pr_review_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<PrReviewGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = pr_review_state_schema();
    let builder = StateGraphBuilder::<PrReviewState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: PrReviewState| async move {
                emit_decision_node_event("pr_review", CLASSIFY, &s.identifier)?;
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
            |s: PrReviewState| async move {
                emit_decision_node_event("pr_review", NO_DATA, &s.identifier)?;
                let mut next = s;
                next.decision = PrReviewDecision::NoData;
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
            HUMAN_REVIEW_RISK,
            |s: PrReviewState| async move {
                emit_decision_node_event("pr_review", HUMAN_REVIEW_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = PrReviewDecision::HumanReviewRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                HUMAN_REVIEW_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            REVIEW_LATENCY_RISK,
            |s: PrReviewState| async move {
                emit_decision_node_event("pr_review", REVIEW_LATENCY_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = PrReviewDecision::ReviewLatencyRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                REVIEW_LATENCY_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            FINDING_LOAD_RISK,
            |s: PrReviewState| async move {
                emit_decision_node_event("pr_review", FINDING_LOAD_RISK, &s.identifier)?;
                let mut next = s;
                next.decision = PrReviewDecision::FindingLoadRisk;
                Ok::<_, NodeError>(next)
            },
            node_config(
                FINDING_LOAD_RISK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            HEALTHY_REVIEW_LOOP,
            |s: PrReviewState| async move {
                emit_decision_node_event("pr_review", HEALTHY_REVIEW_LOOP, &s.identifier)?;
                let mut next = s;
                next.decision = PrReviewDecision::HealthyReviewLoop;
                Ok::<_, NodeError>(next)
            },
            node_config(
                HEALTHY_REVIEW_LOOP,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &PrReviewState| match expected_decision(s) {
            PrReviewDecision::NoData => NO_DATA.into(),
            PrReviewDecision::HumanReviewRisk => HUMAN_REVIEW_RISK.into(),
            PrReviewDecision::ReviewLatencyRisk => REVIEW_LATENCY_RISK.into(),
            PrReviewDecision::FindingLoadRisk => FINDING_LOAD_RISK.into(),
            PrReviewDecision::HealthyReviewLoop => HEALTHY_REVIEW_LOOP.into(),
            PrReviewDecision::Unclassified => NO_DATA.into(),
        })
        .add_edge(NO_DATA, END)
        .add_edge(HUMAN_REVIEW_RISK, END)
        .add_edge(REVIEW_LATENCY_RISK, END)
        .add_edge(FINDING_LOAD_RISK, END)
        .add_edge(HEALTHY_REVIEW_LOOP, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_pr_review_decision_report(
    compiled: &PrReviewGraph,
    state: PrReviewState,
) -> Result<PrReviewGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "pr_review",
        "aggregate",
        &state,
    )?;
    let streamed =
        stream_decision_run(compiled, &thread_id, "pr_review", "aggregate", state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "pr_review",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(PrReviewGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: pr_review_graph_topology(compiled)?,
    })
}

pub fn pr_review_graph_topology(compiled: &PrReviewGraph) -> Result<DecisionGraphTopology, String> {
    topology("pr_review", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::pr_review::{PerRepo, PrReviewReport};

    fn report(human_review_pct: f64, p90: f64, findings_total: u64) -> PrReviewReport {
        PrReviewReport {
            repos: vec!["legatus-ai/sentinel".to_string()],
            window_days: 30,
            total_prs: 4,
            avg_comments_per_pr: 3.0,
            p50_time_to_first_review_hours: 4.0,
            p90_time_to_first_review_hours: p90,
            codex_findings_total: findings_total,
            coderabbit_findings_total: 0,
            human_review_pct,
            per_repo: vec![PerRepo {
                repo: "legatus-ai/sentinel".to_string(),
                prs: 4,
                avg_comments: 3.0,
                p50_ttfr_hours: 4.0,
                p90_ttfr_hours: p90,
                codex_findings: findings_total,
                coderabbit_findings: 0,
                human_review_pct,
            }],
        }
    }

    #[tokio::test]
    async fn graph_authorizes_healthy_review_loop() {
        let graph = build_pr_review_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = PrReviewState::from_report(&report(75.0, 24.0, 4));
        let run = run_pr_review_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, PrReviewDecision::HealthyReviewLoop);
        assert_eq!(run.topology.graph, "pr_review");
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
            .pr_review_authorization()
            .expect("PR review decision should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(
            authorization.decision(),
            PrReviewDecision::HealthyReviewLoop
        );
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_prioritizes_human_review_risk() {
        let graph = build_pr_review_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = PrReviewState::from_report(&report(25.0, 96.0, 40));
        let run = run_pr_review_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, PrReviewDecision::HumanReviewRisk);
    }

    #[tokio::test]
    async fn graph_schema_rejects_broken_totals() {
        let graph = build_pr_review_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = PrReviewState::from_report(&report(75.0, 24.0, 4));
        state.codex_findings_total = 99;

        let err = run_pr_review_decision_report(&graph, state)
            .await
            .expect_err("broken totals should fail schema validation");
        assert!(err.contains("totals"));
    }
}
