//! MCP Tool Handler
//!
//! Routes MCP tool calls (sentinel__*) to the appropriate engine/proof functions.
//! This is how Claude interacts with Sentinel directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use sentinel_domain::proof::ProofEntry;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillWorkflow, WorkflowState};

use crate::cache_efficiency::CacheReport;
use crate::cost_per_point::CostPerPointReport;
use crate::deploy_freq::DeploySummary;
use crate::dev_scorecard::DevScore;
use crate::linear_code_audit::CodeFlag;
use crate::linear_health_score::HealthSummary;
use crate::linear_pm_audit::PmFlag;
use crate::pr_review::PrReviewReport;
use crate::proof_engine::ProofEngine;
use crate::roi::RoiReport;
use crate::severity::SeverityProposal;
use crate::sla::BreachesSummary;
use crate::token_cost::TokenCostSummary;
use crate::tokens::ScanReport;

/// MCP tool call request
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolCall {
    /// Tool name (e.g., "`sentinel__submit_evidence`")
    pub name: String,

    /// Tool arguments as JSON
    pub arguments: serde_json::Value,
}

/// MCP tool call response
#[derive(Debug, Clone, Serialize)]
pub struct McpToolResult {
    /// Whether the call succeeded
    pub success: bool,

    /// Result content
    pub content: serde_json::Value,

    /// Error message if failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One aggregate decision graph run exposed by MCP.
///
/// Used for scanner outputs that produce a single report-level LangGraph
/// decision: tokens, cache efficiency, cost/point, deploy frequency, PR review,
/// ROI, SLA, and token cost.
#[derive(Debug, Clone, Serialize)]
pub struct AggregateGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// MCP proof/step read surface authorized by a durable LangGraph response
/// audit.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpProofReadSurface {
    ProofChain,
    StepProof,
    StepChain,
    ActiveStep,
}

impl McpProofReadSurface {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ProofChain => "proof_chain",
            Self::StepProof => "step_proof",
            Self::StepChain => "step_chain",
            Self::ActiveStep => "active_step",
        }
    }
}

/// Durable graph audit attached to MCP proof/step read responses.
#[derive(Debug, Clone, Serialize)]
pub struct McpProofReadGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub surface: &'static str,
    pub graph_runs_path: PathBuf,
    pub response_sha256: String,
    pub decision: String,
    pub authorization_checkpoint: String,
    pub thread_id: String,
    pub run: serde_json::Value,
}

#[async_trait::async_trait]
pub trait McpProofReadGraphAuditPort: Send + Sync {
    async fn audit_mcp_proof_read(
        &self,
        surface: McpProofReadSurface,
        response: &serde_json::Value,
        graph_jsonl: &Path,
    ) -> anyhow::Result<McpProofReadGraphAudit>;
}

/// MCP request for one BA-Eval benchmark run.
#[derive(Debug, Clone)]
pub struct EvalRunRequest {
    pub run_id: String,
    pub candidates_path: PathBuf,
    pub case_ids: Vec<String>,
    pub corpus_dir: Option<PathBuf>,
    pub runs_dir: Option<PathBuf>,
}

/// Complete MCP eval run response authorized by the eval LangGraph.
#[derive(Debug, Clone, Serialize)]
pub struct EvalRunGraphAudit {
    pub workflow_authority: &'static str,
    pub run: sentinel_domain::eval::EvalRunResult,
    pub graph_audit: AggregateGraphAudit,
}

#[async_trait::async_trait]
pub trait EvalRunGraphPort: Send + Sync {
    async fn run_eval(&self, request: EvalRunRequest) -> anyhow::Result<EvalRunGraphAudit>;
}

/// MCP request for one BA recommendation draft.
#[derive(Debug, Clone)]
pub struct BaDraftGraphRequest {
    pub brief: String,
    pub audience: String,
    pub constraints: Vec<String>,
    pub agent_id: String,
}

/// Complete MCP BA draft response authorized by the BA draft LangGraph.
#[derive(Debug, Clone, Serialize)]
pub struct BaDraftGraphRun {
    pub workflow_authority: &'static str,
    pub recommendation: sentinel_domain::ba::BaRecommendation,
    pub graph_audit: AggregateGraphAudit,
}

#[async_trait::async_trait]
pub trait BaDraftGraphPort: Send + Sync {
    async fn draft_ba_recommendation(
        &self,
        request: BaDraftGraphRequest,
    ) -> anyhow::Result<BaDraftGraphRun>;
}

#[async_trait::async_trait]
pub trait TokenUsageGraphAuditPort: Send + Sync {
    async fn audit_token_usage(
        &self,
        report: &ScanReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

#[async_trait::async_trait]
pub trait CacheEfficiencyGraphAuditPort: Send + Sync {
    async fn audit_cache_efficiency(
        &self,
        report: &CacheReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

#[async_trait::async_trait]
pub trait CostPerPointGraphAuditPort: Send + Sync {
    async fn audit_cost_per_point(
        &self,
        report: &CostPerPointReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

#[async_trait::async_trait]
pub trait DeployFrequencyGraphAuditPort: Send + Sync {
    async fn audit_deploy_frequency(
        &self,
        summary: &DeploySummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

#[async_trait::async_trait]
pub trait PrReviewGraphAuditPort: Send + Sync {
    async fn audit_pr_review(
        &self,
        report: &PrReviewReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

#[async_trait::async_trait]
pub trait RoiGraphAuditPort: Send + Sync {
    async fn audit_roi(
        &self,
        report: &RoiReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

#[async_trait::async_trait]
pub trait SlaGraphAuditPort: Send + Sync {
    async fn audit_sla(
        &self,
        summary: &BreachesSummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit>;
}

/// One read-only severity decision graph run exposed by MCP.
#[derive(Debug, Clone, Serialize)]
pub struct SeverityGraphAuditRun {
    pub identifier: String,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// Read-only LangGraph audit for MCP severity scan proposals.
#[derive(Debug, Clone, Serialize)]
pub struct SeverityGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub proposals_audited: usize,
    pub authorized_sets: usize,
    pub skipped: usize,
    pub runs: Vec<SeverityGraphAuditRun>,
}

/// Infrastructure-owned severity decision graph adapter.
///
/// The application layer owns the MCP response contract but not graph storage
/// construction. Production wires this with sentinel-infrastructure from the
/// CLI bootstrap; tests can use a tiny in-memory implementation.
#[async_trait::async_trait]
pub trait SeverityGraphAuditPort: Send + Sync {
    async fn audit_severity_proposals(
        &self,
        proposals: &[SeverityProposal],
        graph_jsonl: &Path,
    ) -> anyhow::Result<SeverityGraphAudit>;
}

/// One PM audit graph run for a PM-discipline flag exposed by MCP.
#[derive(Debug, Clone, Serialize)]
pub struct PmAuditGraphAuditRun {
    pub identifier: String,
    pub category: String,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// Read-only LangGraph audit for PM-discipline flags.
#[derive(Debug, Clone, Serialize)]
pub struct PmAuditGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub flags_audited: usize,
    pub hard_violations: usize,
    pub advisory_flags: usize,
    pub cleared: usize,
    pub runs: Vec<PmAuditGraphAuditRun>,
}

/// Infrastructure-owned PM audit decision graph adapter. The application layer
/// owns the MCP response contract; production wires the durable LangGraph
/// implementation from the CLI bootstrap.
#[async_trait::async_trait]
pub trait PmAuditGraphAuditPort: Send + Sync {
    async fn audit_pm_flags(
        &self,
        flags: &[PmFlag],
        graph_jsonl: &Path,
    ) -> anyhow::Result<PmAuditGraphAudit>;
}

/// One board-level Linear health graph run exposed by MCP.
#[derive(Debug, Clone, Serialize)]
pub struct LinearHealthGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// Infrastructure-owned Linear health decision graph adapter.
#[async_trait::async_trait]
pub trait LinearHealthGraphAuditPort: Send + Sync {
    async fn audit_linear_health(
        &self,
        summary: &HealthSummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<LinearHealthGraphAudit>;
}

/// One developer scorecard graph run exposed by MCP.
#[derive(Debug, Clone, Serialize)]
pub struct DevScorecardGraphAuditRun {
    pub identifier: String,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// Read-only LangGraph audit for per-developer scorecard rows.
#[derive(Debug, Clone, Serialize)]
pub struct DevScorecardGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub devs_audited: usize,
    pub attribution_divergences: usize,
    pub excellent: usize,
    pub healthy: usize,
    pub needs_attention: usize,
    pub runs: Vec<DevScorecardGraphAuditRun>,
}

/// Infrastructure-owned developer scorecard decision graph adapter.
#[async_trait::async_trait]
pub trait DevScorecardGraphAuditPort: Send + Sync {
    async fn audit_dev_scores(
        &self,
        scores: &[DevScore],
        graph_jsonl: &Path,
    ) -> anyhow::Result<DevScorecardGraphAudit>;
}

/// One aggregate token-cost graph run exposed by MCP.
#[derive(Debug, Clone, Serialize)]
pub struct TokenCostGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// Infrastructure-owned token cost decision graph adapter.
#[async_trait::async_trait]
pub trait TokenCostGraphAuditPort: Send + Sync {
    async fn audit_token_cost(
        &self,
        summary: &TokenCostSummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<TokenCostGraphAudit>;
}

/// One reconciliation graph run for a code-audit flag exposed by MCP.
#[derive(Debug, Clone, Serialize)]
pub struct CodeReconciliationGraphAuditRun {
    pub identifier: String,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

/// Read-only LangGraph reconciliation audit for code-audit flags.
#[derive(Debug, Clone, Serialize)]
pub struct CodeReconciliationGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub flags_audited: usize,
    pub authorized_flags: usize,
    pub cleared: usize,
    pub runs: Vec<CodeReconciliationGraphAuditRun>,
}

/// Infrastructure-owned reconciliation decision graph adapter for code-audit
/// flags. The application layer owns the MCP response shape; production wires
/// the durable LangGraph implementation from the CLI bootstrap.
#[async_trait::async_trait]
pub trait CodeReconciliationAuditPort: Send + Sync {
    async fn audit_code_flags(
        &self,
        flags: &[CodeFlag],
        graph_jsonl: &Path,
    ) -> anyhow::Result<CodeReconciliationGraphAudit>;
}

impl McpToolResult {
    pub const fn ok(content: serde_json::Value) -> Self {
        Self {
            success: true,
            content,
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            success: false,
            content: serde_json::Value::Null,
            error: Some(message.into()),
        }
    }
}

/// Parsed required arguments for `submit_step_complete`.
///
/// Extracted from the raw JSON args by [`parse_submit_step_args`] before
/// the handler touches any `&self` state.  All string fields borrow from
/// the original `args` value; `verdict` is owned (deserialized + sanitized
/// on the way in).
struct SubmitStepArgs<'a> {
    skill: &'a str,
    phase_id: &'a str,
    step_id: &'a str,
    step_description: &'a str,
    verdict: sentinel_domain::judge::JudgeVerdict,
    evidence: sentinel_domain::evidence::Evidence,
    started_at: chrono::DateTime<chrono::Utc>,
}

/// Parse the required fields out of raw MCP args for
/// `sentinel__submit_step_complete`.
///
/// Returns `Err(McpToolResult)` with a user-facing error on any missing or
/// malformed field so the handler body can start at the first decision that
/// actually needs `&self`.
fn parse_submit_step_args(args: &serde_json::Value) -> Result<SubmitStepArgs<'_>, McpToolResult> {
    let skill = args
        .get("skill")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpToolResult::err("Missing 'skill' argument"))?;

    let phase_id = args
        .get("phase_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpToolResult::err("Missing 'phase_id' argument"))?;

    let step_id = args
        .get("step_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpToolResult::err("Missing 'step_id' argument"))?;

    let step_description = args
        .get("step_description")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpToolResult::err("Missing 'step_description' argument"))?;

    let verdict_raw = args
        .get("verdict")
        .cloned()
        .ok_or_else(|| McpToolResult::err("Missing 'verdict' argument"))?;

    let verdict = serde_json::from_value::<sentinel_domain::judge::JudgeVerdict>(verdict_raw)
        .map(sentinel_domain::judge::JudgeVerdict::sanitized)
        .map_err(|e| McpToolResult::err(format!("Invalid 'verdict' shape: {e}")))?;

    let evidence_raw = args
        .get("evidence")
        .cloned()
        .ok_or_else(|| McpToolResult::err("Missing 'evidence' argument"))?;
    let evidence = serde_json::from_value::<sentinel_domain::evidence::Evidence>(evidence_raw)
        .map_err(|e| McpToolResult::err(format!("Invalid 'evidence' shape: {e}")))?;

    let started_at_raw = args
        .get("started_at")
        .and_then(|v| v.as_str())
        .ok_or_else(|| McpToolResult::err("Missing 'started_at' argument"))?;
    let started_at = chrono::DateTime::parse_from_rfc3339(started_at_raw)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| McpToolResult::err(format!("Invalid 'started_at' (expected RFC3339): {e}")))?;

    Ok(SubmitStepArgs {
        skill,
        phase_id,
        step_id,
        step_description,
        verdict,
        evidence,
        started_at,
    })
}

/// MCP handler — routes tool calls to engine functions
pub struct McpHandler {
    state: Arc<RwLock<SessionState>>,
    proof_engine: Arc<ProofEngine>,
    /// Cross-session proof archive backing. `query_proof_corpus` requires this
    /// to be wired and returns an error if a caller omits archive access.
    archive: Option<ProofArchiveBacking>,
    /// Configured workflows used to route step completion through LangGraph.
    /// Without this catalog, `submit_step_complete` fails closed instead of
    /// sealing a proof without durable workflow authority.
    workflows: Option<Arc<HashMap<String, SkillWorkflow>>>,
    /// Optional THE BIBLE evidence-adapter registry (sentinel #68). When
    /// set, `submit_step_complete` calls that pass `evidence_claim` get
    /// a receipt fetched via the registry and folded into
    /// `Evidence.custom.evidence_receipt` before the proof is sealed.
    /// Supplying `evidence_claim` without a registry wired is a fail-fast
    /// error, not a silent skip.
    evidence_adapters: Option<Arc<crate::evidence_adapters::EvidenceAdapterRegistry>>,
    /// Step-level verifier requirements (sentinel #71). Each entry
    /// says "the step at (skill, `phase_id`, `step_id`) cannot seal
    /// unless its evidence carries a receipt from the named
    /// adapter." Checked AFTER the BIBLE wireup folds any
    /// `evidence_claim` receipt into `Evidence.custom`, so verifiers
    /// see the fresh receipt. Production case: require a `browserbase`
    /// adapter receipt at QA-handoff steps so the proof chain
    /// can't lie about smoke-test passes.
    step_verifiers: Vec<sentinel_domain::step_verifier::StepVerifierRequirement>,
    /// Optional LLM port for tools that need a completion (`severity_scan`).
    /// Wired by the CLI's `mcp_cmd` with the `OpenRouterLlm` adapter (the
    /// application layer stays infrastructure-free by holding only the domain
    /// port). `None` ⇒ LLM-backed tools return an error result rather than
    /// silently returning an unscored result.
    llm: Option<Arc<dyn sentinel_domain::ports::LlmPort>>,
    /// Required graph auditor for `sentinel__severity_scan`. The scan itself is
    /// read-only, but every proposal must still pass through the severity
    /// LangGraph so MCP callers receive checkpointed graph decisions.
    severity_graph_auditor: Option<Arc<dyn SeverityGraphAuditPort>>,
    /// Required graph auditor for `sentinel__linear_pm_audit`. PM discipline
    /// flags must be classified by LangGraph before MCP returns them.
    pm_audit_graph_auditor: Option<Arc<dyn PmAuditGraphAuditPort>>,
    /// Required graph auditor for `sentinel__linear_health`. The board-level
    /// health verdict must be checkpointed before MCP returns it.
    linear_health_graph_auditor: Option<Arc<dyn LinearHealthGraphAuditPort>>,
    /// Required graph auditor for `sentinel__dev_scorecard`. Per-developer
    /// scorecard verdicts must be checkpointed before MCP returns them.
    dev_scorecard_graph_auditor: Option<Arc<dyn DevScorecardGraphAuditPort>>,
    /// Required graph auditor for `sentinel__token_cost`. The aggregate cost
    /// verdict must be checkpointed before MCP returns it.
    token_cost_graph_auditor: Option<Arc<dyn TokenCostGraphAuditPort>>,
    /// Required graph auditor for `sentinel__tokens_scan`. Token usage
    /// attribution must be checkpointed before MCP returns it.
    token_usage_graph_auditor: Option<Arc<dyn TokenUsageGraphAuditPort>>,
    /// Required graph auditor for `sentinel__cache_efficiency`. Cache health
    /// must be checkpointed before MCP returns it.
    cache_efficiency_graph_auditor: Option<Arc<dyn CacheEfficiencyGraphAuditPort>>,
    /// Required graph auditor for `sentinel__cost_per_point`. Cost curve
    /// verdicts must be checkpointed before MCP returns them.
    cost_per_point_graph_auditor: Option<Arc<dyn CostPerPointGraphAuditPort>>,
    /// Required graph auditor for `sentinel__deploy_frequency`. DORA cadence
    /// verdicts must be checkpointed before MCP returns them.
    deploy_frequency_graph_auditor: Option<Arc<dyn DeployFrequencyGraphAuditPort>>,
    /// Required graph auditor for `sentinel__pr_review`. Review health
    /// verdicts must be checkpointed before MCP returns them.
    pr_review_graph_auditor: Option<Arc<dyn PrReviewGraphAuditPort>>,
    /// Required graph auditor for `sentinel__roi`. ROI verdicts must be
    /// checkpointed before MCP returns them.
    roi_graph_auditor: Option<Arc<dyn RoiGraphAuditPort>>,
    /// Required graph auditor for `sentinel__sla`. SLA breach verdicts must be
    /// checkpointed before MCP returns them.
    sla_graph_auditor: Option<Arc<dyn SlaGraphAuditPort>>,
    /// Required graph auditor for `sentinel__linear_code_audit`. False-done
    /// flags must be reconciled through LangGraph before MCP returns them.
    code_reconciliation_auditor: Option<Arc<dyn CodeReconciliationAuditPort>>,
    /// Required graph auditor for MCP proof/step read responses. The proof
    /// chain itself is hash-verifiable; this port makes the MCP response
    /// boundary checkpoint-backed too.
    mcp_proof_read_graph_auditor: Option<Arc<dyn McpProofReadGraphAuditPort>>,
    /// Required graph-backed runner for `sentinel__eval_run`. Benchmark runs
    /// must persist both the run result and a durable eval graph verdict.
    eval_runner: Option<Arc<dyn EvalRunGraphPort>>,
    /// Required graph-backed BA draft runner for `sentinel__ba_draft`.
    /// Recommendation drafts must carry durable structure authorization before
    /// MCP returns them.
    ba_draft_runner: Option<Arc<dyn BaDraftGraphPort>>,
}

/// Configuration for cross-session proof corpus reads. Holds the home
/// directory + a filesystem port — together enough to read
/// `<home>/.claude/sentinel/proofs/index.jsonl`.
#[derive(Clone)]
pub struct ProofArchiveBacking {
    pub home: std::path::PathBuf,
    pub fs: std::sync::Arc<dyn sentinel_domain::ports::FileSystemPort>,
}

#[cfg(test)]
fn default_test_workflows() -> HashMap<String, SkillWorkflow> {
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::WorkflowPhase;

    fn workflow(skill: &str, phase_ids: &[&str]) -> SkillWorkflow {
        let phases = phase_ids
            .iter()
            .map(|phase| WorkflowPhase {
                id: (*phase).to_string(),
                file: format!("{phase}.md"),
                required: true,
                judge: JudgeModel::Sonnet,
                description: (*phase).to_string(),
                required_dyad: None,
            })
            .collect();
        SkillWorkflow {
            skill: skill.to_string(),
            phases,
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    let phase_ids = [
        "claim",
        "fetch",
        "intelligence",
        "worktree",
        "review",
        "qa-handoff",
        "cleanup",
    ];
    let mut workflows = HashMap::from([("linear".to_string(), workflow("linear", &phase_ids))]);
    workflows.extend((0..50).map(|i| {
        let skill = format!("stress_skill_{i:03}");
        (skill.clone(), workflow(&skill, &["claim"]))
    }));
    workflows
}

fn load_severity_proposals(path: &Path) -> anyhow::Result<Vec<SeverityProposal>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read severity proposals {}: {e}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            serde_json::from_str::<SeverityProposal>(line).map_err(|e| {
                anyhow::anyhow!(
                    "parse severity proposal {} line {}: {e}",
                    path.display(),
                    idx + 1
                )
            })
        })
        .collect()
}

fn require_graph_workflow_projection<'a>(
    state: &'a SessionState,
    skill: &str,
    tool: &str,
) -> Result<&'a WorkflowState, McpToolResult> {
    state.graph_workflow(skill).ok_or_else(|| {
        McpToolResult::err(format!(
            "{tool} requires LangGraph-projected workflow state for skill '{skill}'; \
             refusing to expose proof data without workflow authority projection"
        ))
    })
}

fn attach_langgraph_workflow_authority(
    mut payload: serde_json::Value,
    workflow_state: &WorkflowState,
) -> Result<serde_json::Value, McpToolResult> {
    let workflow = serde_json::to_value(workflow_state)
        .map_err(|e| McpToolResult::err(format!("Workflow serialization error: {e}")))?;
    let Some(obj) = payload.as_object_mut() else {
        return Err(McpToolResult::err(
            "Serialization error: authority payload did not serialize to an object",
        ));
    };
    obj.insert(
        "workflow_authority".to_string(),
        serde_json::json!("langgraph"),
    );
    obj.insert("graph_workflow".to_string(), workflow);
    Ok(payload)
}

fn mcp_proof_read_graph_jsonl() -> Result<PathBuf, McpToolResult> {
    Ok(mcp_metrics_dir()?.join("mcp-proof-read.graph-runs.jsonl"))
}

fn mcp_claude_dir() -> Result<PathBuf, McpToolResult> {
    crate::paths::try_claude_dir().map_err(|e| {
        McpToolResult::err(format!(
            "could not resolve authoritative Claude directory: {e}"
        ))
    })
}

fn mcp_sentinel_dir() -> Result<PathBuf, McpToolResult> {
    Ok(mcp_claude_dir()?.join("sentinel"))
}

fn mcp_metrics_dir() -> Result<PathBuf, McpToolResult> {
    Ok(mcp_sentinel_dir()?.join("metrics"))
}

#[cfg(test)]
fn mcp_ba_draft_graph_jsonl() -> Result<PathBuf, anyhow::Error> {
    let claude_dir = crate::paths::try_claude_dir()
        .map_err(|e| anyhow::anyhow!("could not resolve authoritative Claude directory: {e}"))?;
    Ok(claude_dir
        .join("sentinel")
        .join("metrics")
        .join("ba-draft")
        .join("mcp-ba-draft.graph-runs.jsonl"))
}

impl McpHandler {
    pub fn new(state: Arc<RwLock<SessionState>>, proof_engine: Arc<ProofEngine>) -> Self {
        Self {
            state,
            proof_engine,
            archive: None,
            workflows: None,
            evidence_adapters: None,
            step_verifiers: Vec::new(),
            llm: None,
            severity_graph_auditor: None,
            pm_audit_graph_auditor: None,
            linear_health_graph_auditor: None,
            dev_scorecard_graph_auditor: None,
            token_cost_graph_auditor: None,
            token_usage_graph_auditor: None,
            cache_efficiency_graph_auditor: None,
            cost_per_point_graph_auditor: None,
            deploy_frequency_graph_auditor: None,
            pr_review_graph_auditor: None,
            roi_graph_auditor: None,
            sla_graph_auditor: None,
            code_reconciliation_auditor: None,
            mcp_proof_read_graph_auditor: None,
            eval_runner: None,
            ba_draft_runner: None,
        }
    }

    async fn attach_mcp_proof_read_authority(
        &self,
        surface: McpProofReadSurface,
        payload: serde_json::Value,
        workflow_state: &WorkflowState,
    ) -> Result<serde_json::Value, McpToolResult> {
        let mut payload = attach_langgraph_workflow_authority(payload, workflow_state)?;
        let Some(graph_auditor) = self.mcp_proof_read_graph_auditor.as_ref() else {
            return Err(McpToolResult::err(format!(
                "{} needs the MCP proof read LangGraph audit port wired at MCP startup",
                surface.label()
            )));
        };
        let graph_jsonl = mcp_proof_read_graph_jsonl()?;
        let graph_audit = graph_auditor
            .audit_mcp_proof_read(surface, &payload, &graph_jsonl)
            .await
            .map_err(|e| {
                McpToolResult::err(format!(
                    "MCP proof read LangGraph audit failed for {}: {e}",
                    surface.label()
                ))
            })?;
        let Some(obj) = payload.as_object_mut() else {
            return Err(McpToolResult::err(
                "Serialization error: MCP proof read payload is not an object",
            ));
        };
        obj.insert("graph_audit".to_string(), serde_json::json!(graph_audit));
        Ok(payload)
    }

    /// Wire the explicit test workflow catalog.
    ///
    /// Kept out of [`Self::new`] so tests exercise the same missing-workflow
    /// posture as production unless they opt in to fixture workflows.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_default_test_workflows(self) -> Self {
        self.with_workflows(default_test_workflows())
    }

    /// Wire configured workflows for graph-backed step submission.
    #[must_use]
    pub fn with_workflows(mut self, workflows: HashMap<String, SkillWorkflow>) -> Self {
        self.workflows = Some(Arc::new(workflows));
        self
    }

    /// Wire an LLM port (for `sentinel__severity_scan`). The CLI's `mcp_cmd`
    /// injects the `OpenRouterLlm` adapter here. Without it, the severity tool
    /// returns an error result (no panic).
    #[must_use]
    pub fn with_llm(mut self, llm: Arc<dyn sentinel_domain::ports::LlmPort>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Wire the read-only severity decision graph auditor used by
    /// `sentinel__severity_scan`.
    #[must_use]
    pub fn with_severity_graph_auditor(mut self, auditor: Arc<dyn SeverityGraphAuditPort>) -> Self {
        self.severity_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only PM audit decision graph auditor used by
    /// `sentinel__linear_pm_audit`.
    #[must_use]
    pub fn with_pm_audit_graph_auditor(mut self, auditor: Arc<dyn PmAuditGraphAuditPort>) -> Self {
        self.pm_audit_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only Linear health graph auditor used by
    /// `sentinel__linear_health`.
    #[must_use]
    pub fn with_linear_health_graph_auditor(
        mut self,
        auditor: Arc<dyn LinearHealthGraphAuditPort>,
    ) -> Self {
        self.linear_health_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only developer scorecard graph auditor used by
    /// `sentinel__dev_scorecard`.
    #[must_use]
    pub fn with_dev_scorecard_graph_auditor(
        mut self,
        auditor: Arc<dyn DevScorecardGraphAuditPort>,
    ) -> Self {
        self.dev_scorecard_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only token cost graph auditor used by
    /// `sentinel__token_cost`.
    #[must_use]
    pub fn with_token_cost_graph_auditor(
        mut self,
        auditor: Arc<dyn TokenCostGraphAuditPort>,
    ) -> Self {
        self.token_cost_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only token-usage graph auditor used by
    /// `sentinel__tokens_scan`.
    #[must_use]
    pub fn with_token_usage_graph_auditor(
        mut self,
        auditor: Arc<dyn TokenUsageGraphAuditPort>,
    ) -> Self {
        self.token_usage_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only cache-efficiency graph auditor used by
    /// `sentinel__cache_efficiency`.
    #[must_use]
    pub fn with_cache_efficiency_graph_auditor(
        mut self,
        auditor: Arc<dyn CacheEfficiencyGraphAuditPort>,
    ) -> Self {
        self.cache_efficiency_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only cost-per-point graph auditor used by
    /// `sentinel__cost_per_point`.
    #[must_use]
    pub fn with_cost_per_point_graph_auditor(
        mut self,
        auditor: Arc<dyn CostPerPointGraphAuditPort>,
    ) -> Self {
        self.cost_per_point_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only deploy-frequency graph auditor used by
    /// `sentinel__deploy_frequency`.
    #[must_use]
    pub fn with_deploy_frequency_graph_auditor(
        mut self,
        auditor: Arc<dyn DeployFrequencyGraphAuditPort>,
    ) -> Self {
        self.deploy_frequency_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only PR-review graph auditor used by
    /// `sentinel__pr_review`.
    #[must_use]
    pub fn with_pr_review_graph_auditor(
        mut self,
        auditor: Arc<dyn PrReviewGraphAuditPort>,
    ) -> Self {
        self.pr_review_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only ROI graph auditor used by `sentinel__roi`.
    #[must_use]
    pub fn with_roi_graph_auditor(mut self, auditor: Arc<dyn RoiGraphAuditPort>) -> Self {
        self.roi_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only SLA graph auditor used by `sentinel__sla`.
    #[must_use]
    pub fn with_sla_graph_auditor(mut self, auditor: Arc<dyn SlaGraphAuditPort>) -> Self {
        self.sla_graph_auditor = Some(auditor);
        self
    }

    /// Wire the read-only reconciliation decision graph auditor used by
    /// `sentinel__linear_code_audit`.
    #[must_use]
    pub fn with_code_reconciliation_auditor(
        mut self,
        auditor: Arc<dyn CodeReconciliationAuditPort>,
    ) -> Self {
        self.code_reconciliation_auditor = Some(auditor);
        self
    }

    /// Wire the MCP proof/step read graph auditor.
    #[must_use]
    pub fn with_mcp_proof_read_graph_auditor(
        mut self,
        auditor: Arc<dyn McpProofReadGraphAuditPort>,
    ) -> Self {
        self.mcp_proof_read_graph_auditor = Some(auditor);
        self
    }

    /// Wire the graph-backed BA-Eval runner used by `sentinel__eval_run`.
    #[must_use]
    pub fn with_eval_runner(mut self, runner: Arc<dyn EvalRunGraphPort>) -> Self {
        self.eval_runner = Some(runner);
        self
    }

    /// Wire the graph-backed BA recommendation drafter used by
    /// `sentinel__ba_draft`.
    #[must_use]
    pub fn with_ba_draft_runner(mut self, runner: Arc<dyn BaDraftGraphPort>) -> Self {
        self.ba_draft_runner = Some(runner);
        self
    }

    /// Register one or more step-level verifier requirements
    /// (sentinel #71). Replaces any previously-set list — call
    /// once at bootstrap with the full set.
    #[must_use]
    pub fn with_step_verifiers(
        mut self,
        verifiers: Vec<sentinel_domain::step_verifier::StepVerifierRequirement>,
    ) -> Self {
        self.step_verifiers = verifiers;
        self
    }

    /// Wire the THE BIBLE evidence-adapter registry. After this,
    /// `submit_step_complete` calls that include `evidence_claim`
    /// in their args go through the registry to fetch a receipt
    /// before sealing the proof. Without this wired, supplying an
    /// `evidence_claim` errors loudly (fail-fast — better than
    /// silently dropping the claim).
    #[must_use]
    pub fn with_evidence_adapters(
        mut self,
        adapters: Arc<crate::evidence_adapters::EvidenceAdapterRegistry>,
    ) -> Self {
        self.evidence_adapters = Some(adapters);
        self
    }

    /// Wire the cross-session proof archive backing. After this,
    /// `query_proof_corpus` returns chains from prior sessions in addition
    /// to live ones, keying by `(session_id, skill)` with live state
    /// winning ties.
    #[must_use]
    pub fn with_archive(mut self, archive: ProofArchiveBacking) -> Self {
        self.archive = Some(archive);
        self
    }

    /// Validate the production MCP runtime has every enterprise authority wired.
    ///
    /// Individual tools still fail closed if a test or custom embedding omits a
    /// dependency, but the CLI/MCP server calls this once at startup so
    /// production cannot launch with a partial LangGraph surface.
    pub fn validate_enterprise_langgraph_runtime(&self) -> anyhow::Result<()> {
        let mut missing = Vec::new();

        if !self.proof_engine.has_phase_graph_authority() {
            missing.push("proof_engine.phase_graph_authority");
        }
        if !self.proof_engine.has_step_graph_authority() {
            missing.push("proof_engine.step_graph_authority");
        }
        if self.workflows.is_none() {
            missing.push("workflow_catalog");
        }
        if self.archive.is_none() {
            missing.push("proof_archive_backing");
        }
        if self.llm.is_none() {
            missing.push("llm_port");
        }
        if self.severity_graph_auditor.is_none() {
            missing.push("severity_graph_auditor");
        }
        if self.pm_audit_graph_auditor.is_none() {
            missing.push("pm_audit_graph_auditor");
        }
        if self.linear_health_graph_auditor.is_none() {
            missing.push("linear_health_graph_auditor");
        }
        if self.dev_scorecard_graph_auditor.is_none() {
            missing.push("dev_scorecard_graph_auditor");
        }
        if self.token_cost_graph_auditor.is_none() {
            missing.push("token_cost_graph_auditor");
        }
        if self.token_usage_graph_auditor.is_none() {
            missing.push("token_usage_graph_auditor");
        }
        if self.cache_efficiency_graph_auditor.is_none() {
            missing.push("cache_efficiency_graph_auditor");
        }
        if self.cost_per_point_graph_auditor.is_none() {
            missing.push("cost_per_point_graph_auditor");
        }
        if self.deploy_frequency_graph_auditor.is_none() {
            missing.push("deploy_frequency_graph_auditor");
        }
        if self.pr_review_graph_auditor.is_none() {
            missing.push("pr_review_graph_auditor");
        }
        if self.roi_graph_auditor.is_none() {
            missing.push("roi_graph_auditor");
        }
        if self.sla_graph_auditor.is_none() {
            missing.push("sla_graph_auditor");
        }
        if self.code_reconciliation_auditor.is_none() {
            missing.push("code_reconciliation_auditor");
        }
        if self.mcp_proof_read_graph_auditor.is_none() {
            missing.push("mcp_proof_read_graph_auditor");
        }
        if self.eval_runner.is_none() {
            missing.push("eval_runner");
        }
        if self.ba_draft_runner.is_none() {
            missing.push("ba_draft_runner");
        }

        if !missing.is_empty() {
            anyhow::bail!(
                "MCP enterprise LangGraph runtime missing required authorities: {}",
                missing.join(", ")
            );
        }

        Ok(())
    }

    /// Handle an MCP tool call
    pub async fn handle(&self, call: McpToolCall) -> McpToolResult {
        match call.name.as_str() {
            "sentinel__get_proof_chain" => self.get_proof_chain(call.arguments).await,
            "sentinel__get_workflow_status" => self.get_workflow_status(call.arguments).await,
            "sentinel__verify_chain" => self.verify_chain(call.arguments).await,
            // ── Step-level (M4.1) ─────────────────────────────────────
            "sentinel__get_step_proof" => self.get_step_proof(call.arguments).await,
            "sentinel__get_step_chain" => self.get_step_chain(call.arguments).await,
            "sentinel__get_active_step" => self.get_active_step(call.arguments).await,
            // ── Step-level write (M4.2) ──────────────────────────────
            "sentinel__submit_step_complete" => self.submit_step_complete(call.arguments).await,
            // ── Proof corpus query (M4.3) ────────────────────────────
            "sentinel__query_proof_corpus" => self.query_proof_corpus(call.arguments).await,
            "sentinel__linear_pm_audit" => self.linear_pm_audit(call.arguments).await,
            "sentinel__severity_scan" => self.severity_scan().await,
            "sentinel__dev_scorecard" => self.dev_scorecard(call.arguments).await,
            "sentinel__linear_code_audit" => self.linear_code_audit(call.arguments).await,
            "sentinel__linear_health" => self.linear_health(call.arguments).await,
            "sentinel__tokens_scan" => self.tokens_scan().await,
            "sentinel__cache_efficiency" => self.cache_efficiency().await,
            "sentinel__cost_per_point" => self.cost_per_point().await,
            "sentinel__deploy_frequency" => self.deploy_frequency().await,
            "sentinel__pr_review" => self.pr_review(call.arguments).await,
            "sentinel__roi" => self.roi().await,
            "sentinel__sla" => self.sla().await,
            "sentinel__token_cost" => self.token_cost().await,
            "sentinel__eval_run" => self.eval_run(call.arguments).await,
            "sentinel__ba_draft" => self.ba_draft(call.arguments).await,
            _ => McpToolResult::err(format!("Unknown tool: {}", call.name)),
        }
    }

    async fn get_proof_chain(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let (chain, workflow_state) = {
            let state = self.state.read().await;
            let chain = match state.proof_chain(skill) {
                Some(chain) => chain.clone(),
                None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
            };
            let workflow_state =
                match require_graph_workflow_projection(&state, skill, "sentinel__get_proof_chain")
                {
                    Ok(workflow_state) => workflow_state,
                    Err(err) => return err,
                };
            (chain, workflow_state.clone())
        };
        let payload = match serde_json::to_value(chain) {
            Ok(v) => v,
            Err(e) => return McpToolResult::err(format!("Serialization error: {e}")),
        };
        match self
            .attach_mcp_proof_read_authority(
                McpProofReadSurface::ProofChain,
                payload,
                &workflow_state,
            )
            .await
        {
            Ok(payload) => McpToolResult::ok(payload),
            Err(err) => err,
        }
    }

    async fn get_workflow_status(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let state = self.state.read().await;
        match state.graph_workflow(skill) {
            Some(wf) => match serde_json::to_value(wf) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            None => McpToolResult::err(format!(
                "No LangGraph-projected workflow state for skill '{skill}'"
            )),
        }
    }

    async fn verify_chain(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        match self.proof_engine.verify_chain(skill).await {
            Ok(verification) => match serde_json::to_value(&verification) {
                Ok(v) => McpToolResult::ok(v),
                Err(e) => McpToolResult::err(format!("Serialization error: {e}")),
            },
            Err(e) => McpToolResult::err(format!("Verification failed: {e}")),
        }
    }

    /// `sentinel__linear_pm_audit` — run the PM-enforcement audit over the
    /// local Linear issue cache, then classify every PM-discipline flag through
    /// the PM audit LangGraph. Read-only; writes scan and graph artifacts.
    async fn linear_pm_audit(&self, args: serde_json::Value) -> McpToolResult {
        use crate::linear_pm_audit::{scan_pm_audit_report, BurndownInputs};

        let Some(graph_auditor) = self.pm_audit_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "linear_pm_audit needs a PM audit LangGraph audit port wired at MCP startup",
            );
        };
        let sentinel_dir = match mcp_sentinel_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let linear_cache = sentinel_dir.join("linear-assigned.json");
        let output_summary = sentinel_dir.join("metrics").join("linear-pm-audit.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        let burndown = BurndownInputs {
            velocity_pts_per_week: args
                .get("velocity_pts_per_week")
                .and_then(serde_json::Value::as_f64),
            weeks_available: args
                .get("weeks_available")
                .and_then(serde_json::Value::as_f64),
        };

        match scan_pm_audit_report(&linear_cache, &output_summary, burndown) {
            Ok(report) => match graph_auditor
                .audit_pm_flags(&report.flags, &graph_jsonl)
                .await
            {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report.summary,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("PM audit LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("PM audit failed: {e}")),
        }
    }

    /// `sentinel__severity_scan` — LLM-judge each cached ticket's priority
    /// (Opus 4.8 + GPT-5.5, reconciled), then run every proposal through the
    /// read-only severity LangGraph and return both the scan summary and graph
    /// audit. It never applies a Linear mutation; MCP still gets durable graph
    /// decisions instead of a non-graph report.
    async fn severity_scan(&self) -> McpToolResult {
        use crate::severity::scan_severity;

        let Some(llm) = self.llm.as_ref() else {
            return McpToolResult::err(
                "severity_scan needs an LLM port (set OPENROUTER_API_KEY so the MCP server wires \
                 OpenRouterLlm)",
            );
        };
        let Some(graph_auditor) = self.severity_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "severity_scan needs a severity LangGraph audit port wired at MCP startup",
            );
        };
        let sentinel_dir = match mcp_sentinel_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let linear_cache = sentinel_dir.join("linear-assigned.json");
        let output = sentinel_dir.join("metrics").join("severity.json");
        let proposals_jsonl = output.with_extension("jsonl");
        let graph_jsonl = output.with_extension("graph-runs.jsonl");

        match scan_severity(&linear_cache, &output, llm.as_ref()).await {
            Ok(summary) => {
                let proposals = match load_severity_proposals(&proposals_jsonl) {
                    Ok(proposals) => proposals,
                    Err(e) => {
                        return McpToolResult::err(format!("Severity proposal load failed: {e}"));
                    }
                };
                let graph_audit = match graph_auditor
                    .audit_severity_proposals(&proposals, &graph_jsonl)
                    .await
                {
                    Ok(audit) => audit,
                    Err(e) => {
                        return McpToolResult::err(format!("Severity LangGraph audit failed: {e}"));
                    }
                };
                McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": summary,
                    "graph_audit": graph_audit,
                }))
            }
            Err(e) => McpToolResult::err(format!("Severity scan failed: {e}")),
        }
    }

    /// `sentinel__dev_scorecard` — compute per-developer scorecards from the
    /// git-stats input + the Linear cache, then run every per-dev row through
    /// the developer scorecard LangGraph. Read-only; writes scan and graph
    /// artifacts.
    async fn dev_scorecard(&self, _args: serde_json::Value) -> McpToolResult {
        use crate::dev_scorecard::scan_dev_scorecard;

        let Some(graph_auditor) = self.dev_scorecard_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "dev_scorecard needs a developer scorecard LangGraph audit port wired at MCP startup",
            );
        };
        let sentinel_dir = match mcp_sentinel_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let git_stats = sentinel_dir.join("dev-git-stats.json");
        let linear_cache = sentinel_dir.join("linear-assigned.json");
        let output_summary = sentinel_dir.join("metrics").join("dev-scorecard.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        match scan_dev_scorecard(&git_stats, &linear_cache, &output_summary) {
            Ok(summary) => match graph_auditor
                .audit_dev_scores(&summary.devs, &graph_jsonl)
                .await
            {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": summary,
                    "graph_audit": graph_audit,
                })),
                Err(e) => {
                    McpToolResult::err(format!("Developer scorecard LangGraph audit failed: {e}"))
                }
            },
            Err(e) => McpToolResult::err(format!("Dev scorecard failed: {e}")),
        }
    }

    /// `sentinel__linear_code_audit` — cross-check every Completed ticket in
    /// the Linear cache against the precomputed code-evidence map, then route
    /// every false-done flag through the reconciliation LangGraph. Read-only;
    /// writes both scan and graph-audit artifacts.
    async fn linear_code_audit(&self, _args: serde_json::Value) -> McpToolResult {
        use crate::linear_code_audit::scan_code_audit;

        let Some(graph_auditor) = self.code_reconciliation_auditor.as_ref() else {
            return McpToolResult::err(
                "linear_code_audit needs a reconciliation LangGraph audit port wired at MCP startup",
            );
        };
        let sentinel_dir = match mcp_sentinel_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let linear_cache = sentinel_dir.join("linear-assigned.json");
        let evidence_map = sentinel_dir.join("ticket-code-evidence.json");
        let output_summary = sentinel_dir.join("metrics").join("linear-code-audit.json");
        let graph_jsonl = output_summary.with_extension("reconciliation-graph-runs.jsonl");

        match scan_code_audit(&linear_cache, &evidence_map, &output_summary) {
            Ok(summary) => match graph_auditor
                .audit_code_flags(&summary.flags, &graph_jsonl)
                .await
            {
                Ok(reconciliation_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": summary,
                    "reconciliation_audit": reconciliation_audit,
                })),
                Err(e) => {
                    McpToolResult::err(format!("Code reconciliation LangGraph audit failed: {e}"))
                }
            },
            Err(e) => McpToolResult::err(format!("Code audit failed: {e}")),
        }
    }

    /// `sentinel__linear_health` — compute the composite 0-100 Linear health
    /// score, then run the board-level verdict through the Linear health
    /// LangGraph. Read-only; writes scan and graph artifacts.
    async fn linear_health(&self, _args: serde_json::Value) -> McpToolResult {
        use crate::linear_health_score::scan_health_score;

        let Some(graph_auditor) = self.linear_health_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "linear_health needs a Linear health LangGraph audit port wired at MCP startup",
            );
        };
        let sentinel_dir = match mcp_sentinel_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let linear_cache = sentinel_dir.join("linear-assigned.json");
        let output_summary = sentinel_dir.join("metrics").join("linear-health.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        match scan_health_score(&linear_cache, &output_summary) {
            Ok(summary) => match graph_auditor
                .audit_linear_health(&summary, &graph_jsonl)
                .await
            {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": summary,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("Linear health LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("Health score failed: {e}")),
        }
    }

    /// `sentinel__tokens_scan` — aggregate session token usage by ticket, then
    /// validate the attribution report through the token-usage LangGraph.
    async fn tokens_scan(&self) -> McpToolResult {
        use crate::tokens::scan_token_usage;

        let Some(graph_auditor) = self.token_usage_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "tokens_scan needs a token usage LangGraph audit port wired at MCP startup",
            );
        };
        let claude_dir = match mcp_claude_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let metrics = match mcp_metrics_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let projects = claude_dir.join("projects");
        let output = metrics.join("tokens-per-ticket.jsonl");
        let graph_jsonl = output.with_extension("graph-runs.jsonl");

        match scan_token_usage(&projects, &output) {
            Ok(report) => match graph_auditor.audit_token_usage(&report, &graph_jsonl).await {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("Token usage LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("Token usage scan failed: {e}")),
        }
    }

    /// `sentinel__cache_efficiency` — scan prompt-cache hit rates, then
    /// validate the report through the cache-efficiency LangGraph.
    async fn cache_efficiency(&self) -> McpToolResult {
        use crate::cache_efficiency::scan_cache_efficiency;

        let Some(graph_auditor) = self.cache_efficiency_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "cache_efficiency needs a cache efficiency LangGraph audit port wired at MCP startup",
            );
        };
        let claude_dir = match mcp_claude_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let metrics = match mcp_metrics_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let projects = claude_dir.join("projects");
        let output_jsonl = metrics.join("cache-efficiency.jsonl");
        let output_summary = metrics.join("cache-efficiency-summary.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        match scan_cache_efficiency(&projects, &output_jsonl, &output_summary) {
            Ok(report) => match graph_auditor
                .audit_cache_efficiency(&report, &graph_jsonl)
                .await
            {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => {
                    McpToolResult::err(format!("Cache efficiency LangGraph audit failed: {e}"))
                }
            },
            Err(e) => McpToolResult::err(format!("Cache efficiency scan failed: {e}")),
        }
    }

    /// `sentinel__cost_per_point` — join token usage and Linear estimates,
    /// then validate the cost curve through the cost-per-point LangGraph.
    async fn cost_per_point(&self) -> McpToolResult {
        use crate::cost_per_point::scan_cost_per_point;

        let Some(graph_auditor) = self.cost_per_point_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "cost_per_point needs a cost-per-point LangGraph audit port wired at MCP startup",
            );
        };
        let sentinel_dir = match mcp_sentinel_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let metrics = sentinel_dir.join("metrics");
        let tokens_input = metrics.join("tokens-per-ticket.jsonl");
        let linear_cache = sentinel_dir.join("linear-assigned.json");
        let output_jsonl = metrics.join("cost-per-point.jsonl");
        let output_summary = metrics.join("cost-per-point-summary.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        match scan_cost_per_point(&tokens_input, &linear_cache, &output_jsonl, &output_summary) {
            Ok(report) => match graph_auditor
                .audit_cost_per_point(&report, &graph_jsonl)
                .await
            {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("Cost-per-point LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("Cost-per-point scan failed: {e}")),
        }
    }

    /// `sentinel__deploy_frequency` — aggregate deployment events, then
    /// validate the DORA cadence verdict through the deploy-frequency graph.
    async fn deploy_frequency(&self) -> McpToolResult {
        use crate::deploy_freq::aggregate;

        let Some(graph_auditor) = self.deploy_frequency_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "deploy_frequency needs a deploy frequency LangGraph audit port wired at MCP startup",
            );
        };
        let metrics = match mcp_metrics_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let deploys = metrics.join("deploys.jsonl");
        let summary = metrics.join("deploys-summary.json");
        let graph_jsonl = summary.with_extension("graph-runs.jsonl");

        match aggregate(&deploys, &summary) {
            Ok(report) => match graph_auditor
                .audit_deploy_frequency(&report, &graph_jsonl)
                .await
            {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => {
                    McpToolResult::err(format!("Deploy frequency LangGraph audit failed: {e}"))
                }
            },
            Err(e) => McpToolResult::err(format!("Deploy frequency aggregate failed: {e}")),
        }
    }

    /// `sentinel__pr_review` — scan merged PR review health, then validate the
    /// aggregate through the PR-review LangGraph.
    async fn pr_review(&self, args: serde_json::Value) -> McpToolResult {
        use crate::pr_review::{scan_pr_reviews, DEFAULT_REPOS};

        let Some(graph_auditor) = self.pr_review_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "pr_review needs a PR review LangGraph audit port wired at MCP startup",
            );
        };
        let Some(output_dir) = crate::pr_review::default_output_dir() else {
            return McpToolResult::err("could not resolve PR review output directory");
        };
        let window_days = args
            .get("window_days")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(30);
        let graph_jsonl = output_dir.join("pr-review-summary.graph-runs.jsonl");
        let repos: Vec<&str> = DEFAULT_REPOS.to_vec();

        match scan_pr_reviews(window_days, &repos, &output_dir) {
            Ok(report) => match graph_auditor.audit_pr_review(&report, &graph_jsonl).await {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("PR review LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("PR review scan failed: {e}")),
        }
    }

    /// `sentinel__roi` — compute ROI against the human-team baseline, then
    /// validate the headline through the ROI LangGraph.
    async fn roi(&self) -> McpToolResult {
        use crate::roi::scan_roi;

        let Some(graph_auditor) = self.roi_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "roi needs an ROI LangGraph audit port wired at MCP startup",
            );
        };
        let metrics = match mcp_metrics_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let tokens_input = metrics.join("tokens-per-ticket.jsonl");
        let cost_per_point_summary = metrics.join("cost-per-point-summary.json");
        let output_jsonl = metrics.join("roi.jsonl");
        let output_summary = metrics.join("roi-summary.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        match scan_roi(
            &tokens_input,
            &cost_per_point_summary,
            &output_jsonl,
            &output_summary,
        ) {
            Ok(report) => match graph_auditor.audit_roi(&report, &graph_jsonl).await {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("ROI LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("ROI scan failed: {e}")),
        }
    }

    /// `sentinel__sla` — aggregate SLA breach records, then validate the
    /// operations verdict through the SLA LangGraph.
    async fn sla(&self) -> McpToolResult {
        use crate::sla::aggregate;

        let Some(graph_auditor) = self.sla_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "sla needs an SLA LangGraph audit port wired at MCP startup",
            );
        };
        let metrics = match mcp_metrics_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let breaches = metrics.join("sla-breaches.jsonl");
        let summary = metrics.join("sla-breaches-summary.json");
        let graph_jsonl = summary.with_extension("graph-runs.jsonl");

        match aggregate(&breaches, &summary) {
            Ok(report) => match graph_auditor.audit_sla(&report, &graph_jsonl).await {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": report,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("SLA LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("SLA aggregate failed: {e}")),
        }
    }

    /// `sentinel__token_cost` — price SEN-7 token aggregates, then run the
    /// aggregate cost verdict through the token cost LangGraph. Read-only;
    /// writes scan and graph artifacts.
    async fn token_cost(&self) -> McpToolResult {
        use crate::token_cost::scan_token_cost;

        let Some(graph_auditor) = self.token_cost_graph_auditor.as_ref() else {
            return McpToolResult::err(
                "token_cost needs a token cost LangGraph audit port wired at MCP startup",
            );
        };
        let metrics = match mcp_metrics_dir() {
            Ok(path) => path,
            Err(err) => return err,
        };
        let tokens_input = metrics.join("tokens-per-ticket.jsonl");
        let output_summary = metrics.join("token-cost.json");
        let graph_jsonl = output_summary.with_extension("graph-runs.jsonl");

        match scan_token_cost(&tokens_input, &output_summary) {
            Ok(summary) => match graph_auditor.audit_token_cost(&summary, &graph_jsonl).await {
                Ok(graph_audit) => McpToolResult::ok(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "summary": summary,
                    "graph_audit": graph_audit,
                })),
                Err(e) => McpToolResult::err(format!("Token cost LangGraph audit failed: {e}")),
            },
            Err(e) => McpToolResult::err(format!("Token cost failed: {e}")),
        }
    }

    /// `sentinel__eval_run` — execute a BA-Eval benchmark from explicit
    /// candidate artifacts, then authorize the aggregate verdict through the
    /// durable eval LangGraph. Production infrastructure owns corpus/scorer/
    /// store construction; MCP owns the tool contract and fail-closed posture.
    async fn eval_run(&self, args: serde_json::Value) -> McpToolResult {
        let Some(runner) = self.eval_runner.as_ref() else {
            return McpToolResult::err(
                "eval_run needs an eval LangGraph run port wired at MCP startup",
            );
        };
        let run_id = match args.get("run_id").and_then(serde_json::Value::as_str) {
            Some(value) if !value.trim().is_empty() => value.trim().to_string(),
            _ => return McpToolResult::err("Missing 'run_id' argument"),
        };
        let candidates_path = match args
            .get("candidates_path")
            .and_then(serde_json::Value::as_str)
        {
            Some(value) if !value.trim().is_empty() => PathBuf::from(value),
            _ => return McpToolResult::err("Missing 'candidates_path' argument"),
        };
        let case_ids = match args.get("case_ids") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(value) => {
                let Some(values) = value.as_array() else {
                    return McpToolResult::err("'case_ids' must be an array of strings");
                };
                let mut out = Vec::with_capacity(values.len());
                for (idx, value) in values.iter().enumerate() {
                    let Some(case_id) = value.as_str() else {
                        return McpToolResult::err(format!("'case_ids[{idx}]' must be a string"));
                    };
                    out.push(case_id.to_string());
                }
                out
            }
        };
        let corpus_dir = match args.get("corpus_dir") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => match value.as_str() {
                Some(path) if !path.trim().is_empty() => Some(PathBuf::from(path)),
                _ => return McpToolResult::err("'corpus_dir' must be a non-empty string"),
            },
        };
        let runs_dir = match args.get("runs_dir") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => match value.as_str() {
                Some(path) if !path.trim().is_empty() => Some(PathBuf::from(path)),
                _ => return McpToolResult::err("'runs_dir' must be a non-empty string"),
            },
        };

        match runner
            .run_eval(EvalRunRequest {
                run_id,
                candidates_path,
                case_ids,
                corpus_dir,
                runs_dir,
            })
            .await
        {
            Ok(audit) => McpToolResult::ok(serde_json::json!({
                "workflow_authority": audit.workflow_authority,
                "run": audit.run,
                "graph_audit": audit.graph_audit,
            })),
            Err(e) => McpToolResult::err(format!("Eval LangGraph run failed: {e}")),
        }
    }

    /// `sentinel__ba_draft` — produce a BA recommendation envelope, then
    /// authorize the draft structure through the durable BA draft LangGraph.
    async fn ba_draft(&self, args: serde_json::Value) -> McpToolResult {
        let Some(runner) = self.ba_draft_runner.as_ref() else {
            return McpToolResult::err(
                "ba_draft needs a BA draft LangGraph run port wired at MCP startup",
            );
        };
        let brief = match args.get("brief").and_then(serde_json::Value::as_str) {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => return McpToolResult::err("Missing 'brief' argument"),
        };
        let audience = match args.get("audience").and_then(serde_json::Value::as_str) {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => return McpToolResult::err("Missing 'audience' argument"),
        };
        let constraints = match args.get("constraints") {
            None | Some(serde_json::Value::Null) => Vec::new(),
            Some(value) => {
                let Some(values) = value.as_array() else {
                    return McpToolResult::err("'constraints' must be an array of strings");
                };
                let mut out = Vec::with_capacity(values.len());
                for (idx, value) in values.iter().enumerate() {
                    let Some(constraint) = value.as_str() else {
                        return McpToolResult::err(format!(
                            "'constraints[{idx}]' must be a string"
                        ));
                    };
                    out.push(constraint.to_string());
                }
                out
            }
        };
        let agent_id = args
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("ba-orchestrator")
            .to_string();

        match runner
            .draft_ba_recommendation(BaDraftGraphRequest {
                brief,
                audience,
                constraints,
                agent_id,
            })
            .await
        {
            Ok(result) => McpToolResult::ok(serde_json::json!({
                "workflow_authority": result.workflow_authority,
                "recommendation": result.recommendation,
                "graph_audit": result.graph_audit,
            })),
            Err(e) => McpToolResult::err(format!("BA draft LangGraph run failed: {e}")),
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Step-level tools (M4.1)
    //
    // These three tools expose the step-level chain that M1.1-M1.5
    // built. Together they give external MCP servers (skills-mcp,
    // agents-mcp, local clients) a clean read surface against the chain
    // without needing to mirror sentinel's serialization format.
    //
    // - get_step_proof(skill, step_id [, phase_id]) → single StepProof
    // - get_step_chain(skill) → ordered list of step entries with
    //   verification status (the chain restricted to step entries)
    // - get_active_step(skill) → which step is "next" to run
    //   (skill's chain head + the immediate next step from config,
    //   if config is loaded into state)
    // ─────────────────────────────────────────────────────────────────

    /// Return a single [`StepProof`](sentinel_domain::step_proof::StepProof)
    /// matching `(skill, step_id [, phase_id])`. Phase id disambiguates
    /// when the same `step_id` repeats across phases (e.g. "1" in both
    /// "claim" and "review" phases).
    async fn get_step_proof(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };
        let step_id = match args.get("step_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'step_id' argument"),
        };
        // phase_id is optional — only required when the chain has step_ids
        // that collide across phases (uncommon but possible).
        let phase_filter = args.get("phase_id").and_then(|v| v.as_str());

        let (found, workflow_state) = {
            let state = self.state.read().await;
            let chain = match state.proof_chain(skill) {
                Some(c) => c,
                None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
            };
            let workflow_state = match require_graph_workflow_projection(
                &state,
                skill,
                "sentinel__get_step_proof",
            ) {
                Ok(workflow_state) => workflow_state.clone(),
                Err(err) => return err,
            };

            // Walk the mixed-entry chain looking for a matching step entry.
            // We search in reverse so the *most recent* matching step wins
            // when an idempotent step_id has been re-recorded — important
            // for replay/resubmission semantics.
            let found = chain.entries.iter().rev().find_map(|e| match e {
                ProofEntry::Step(s) if s.step_id == step_id => match phase_filter {
                    Some(p) if s.phase_id != p => None,
                    _ => Some(s.clone()),
                },
                _ => None,
            });
            (found, workflow_state)
        };

        match found {
            Some(proof) => {
                let payload = match serde_json::to_value(&proof) {
                    Ok(v) => v,
                    Err(e) => return McpToolResult::err(format!("Serialization error: {e}")),
                };
                match self
                    .attach_mcp_proof_read_authority(
                        McpProofReadSurface::StepProof,
                        payload,
                        &workflow_state,
                    )
                    .await
                {
                    Ok(payload) => McpToolResult::ok(payload),
                    Err(err) => err,
                }
            }
            None => McpToolResult::err(format!(
                "No StepProof for skill '{skill}', step_id '{step_id}'{}",
                phase_filter
                    .map(|p| format!(" (phase '{p}')"))
                    .unwrap_or_default(),
            )),
        }
    }

    /// Return all step entries from the chain for a skill, in order.
    /// Phase entries are filtered out — callers wanting the full
    /// mixed chain should use `sentinel__get_proof_chain`.
    ///
    /// Response shape:
    /// ```json
    /// {
    ///   "skill": "linear",
    ///   "session_id": "...",
    ///   "step_count": 3,
    ///   "head_hash": "...",
    ///   "steps": [ {step_id, phase_id, combined_hash, ...}, ... ]
    /// }
    /// ```
    async fn get_step_chain(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let (chain, workflow_state) = {
            let state = self.state.read().await;
            let chain = match state.proof_chain(skill) {
                Some(c) => c.clone(),
                None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
            };
            let workflow_state = match require_graph_workflow_projection(
                &state,
                skill,
                "sentinel__get_step_chain",
            ) {
                Ok(workflow_state) => workflow_state.clone(),
                Err(err) => return err,
            };
            (chain, workflow_state)
        };

        let steps: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();
        let response_skill = chain.skill.clone();
        let response_session_id = chain.session_id.clone();
        let head_hash = chain.head_hash().to_string();

        let payload = serde_json::json!({
            "skill": response_skill,
            "session_id": response_session_id,
            "step_count": steps.len(),
            "head_hash": head_hash,
            "steps": steps,
        });
        match self
            .attach_mcp_proof_read_authority(
                McpProofReadSurface::StepChain,
                payload,
                &workflow_state,
            )
            .await
        {
            Ok(payload) => McpToolResult::ok(payload),
            Err(err) => err,
        }
    }

    /// Return the chain's "active step" for a skill — i.e. the head of
    /// the chain plus a hint at what's expected next.
    ///
    /// Response shape:
    /// ```json
    /// {
    ///   "skill": "linear",
    ///   "head_hash": "...",
    ///   "last_step": { "phase_id": "claim", "step_id": "2", ... } | null,
    ///   "chain_length": 5,
    ///   "phase_count": 1,
    ///   "step_count": 4
    /// }
    /// ```
    async fn get_active_step(&self, args: serde_json::Value) -> McpToolResult {
        let skill = match args.get("skill").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return McpToolResult::err("Missing 'skill' argument"),
        };

        let (chain, workflow_state) = {
            let state = self.state.read().await;
            let chain = match state.proof_chain(skill) {
                Some(c) => c.clone(),
                None => return McpToolResult::err(format!("No proof chain for skill '{skill}'")),
            };
            let workflow_state =
                match require_graph_workflow_projection(&state, skill, "sentinel__get_active_step")
                {
                    Ok(workflow_state) => workflow_state.clone(),
                    Err(err) => return err,
                };
            (chain, workflow_state)
        };

        let phase_count = chain.phase_count();
        let step_count = chain.step_count();
        let response_session_id = chain.session_id.clone();
        let head_hash = chain.head_hash().to_string();
        let chain_length = chain.entry_count();

        // last_step = last Step entry in the canonical chain (None if no step
        // entries have sealed yet).
        let last_step = chain.entries.iter().rev().find_map(|e| match e {
            ProofEntry::Step(s) => Some(serde_json::json!({
                "phase_id": s.phase_id,
                "step_id": s.step_id,
                "combined_hash": s.combined_hash,
                "completed_at": s.completed_at,
            })),
            _ => None,
        });

        let payload = serde_json::json!({
            "skill": skill,
            "session_id": response_session_id,
            "head_hash": head_hash,
            "last_step": last_step,
            "chain_length": chain_length,
            "phase_count": phase_count,
            "step_count": step_count,
        });
        match self
            .attach_mcp_proof_read_authority(
                McpProofReadSurface::ActiveStep,
                payload,
                &workflow_state,
            )
            .await
        {
            Ok(payload) => McpToolResult::ok(payload),
            Err(err) => err,
        }
    }

    /// Seal a judged step into the proof chain (M4.2).
    ///
    /// Wraps [`ProofEngine::submit_step_evidence`] so external MCP
    /// servers (skills-mcp, agents-mcp) can advance the chain remotely
    /// without needing direct access to sentinel-application internals.
    ///
    /// Required arguments:
    /// - `skill` (string)
    /// - `phase_id` (string)
    /// - `step_id` (string)
    /// - `step_description` (string) — what "sufficient" means for this step
    /// - `verdict` (object) — `JudgeVerdict` { sufficient, confidence, reasoning, `requested_evidence`? }
    /// - `started_at` (RFC3339 string) — authoritative step start timestamp
    /// - `evidence` (object) — explicit structured proof evidence
    ///
    /// Optional arguments:
    /// - `artifact` (any JSON value) — defaults to null
    /// - `account_context` (string|null) — defaults to null
    ///
    /// Returns the sealed `StepProof` on success, or an error on
    /// insufficient verdict / chain-link mismatch / serialization
    /// failure. Refusing to seal an insufficient verdict is the
    /// engine's job — surface the error here for caller telemetry.
    async fn submit_step_complete(&self, args: serde_json::Value) -> McpToolResult {
        // Parse and validate the required fields up front.
        // Errors here are purely about arg shape — no &self access needed.
        let parsed = match parse_submit_step_args(&args) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let (skill, phase_id, step_id, step_description, verdict) = (
            parsed.skill,
            parsed.phase_id,
            parsed.step_id,
            parsed.step_description,
            parsed.verdict,
        );
        let mut evidence = parsed.evidence;
        let started_at = parsed.started_at;

        // #12 — close the self-certify gap: the caller SUPPLIES the `verdict`
        // arg, so an agent could pass `sufficient: true` regardless of reality.
        // The independently recorded `step_judge` verdict is mandatory and is
        // the only verdict allowed to decide StepProof sealing.
        let independent_verdict = {
            let st = self.state.read().await;
            st.independent_verdict(skill, phase_id, step_id).cloned()
        };
        let Some(independent_verdict) = independent_verdict else {
            return McpToolResult::err(format!(
                "Refusing to seal step '{step_id}' of '{skill}/{phase_id}': \
                 missing independent step_judge verdict. Caller-supplied \
                 verdicts are not accepted as a substitute; rerun the step through \
                 the judged hook path and resubmit."
            ));
        };
        let verdict = sentinel_domain::judge::JudgeVerdict {
            sufficient: independent_verdict.sufficient,
            confidence: independent_verdict.confidence,
            reasoning: format!(
                "Independent step_judge verdict{}; caller reasoning retained for context: {}",
                independent_verdict
                    .judged_at
                    .map(|ts| format!(" recorded at {}", ts.to_rfc3339()))
                    .unwrap_or_default(),
                verdict.reasoning
            ),
            requested_evidence: verdict.requested_evidence.clone(),
        };

        // THE BIBLE: optional `evidence_claim` arg dispatches through the
        // evidence-adapter registry, fetches a receipt, and folds it into
        // `evidence.custom.evidence_receipt`. Without an adapter registry
        // wired on this handler, supplying `evidence_claim` is an error —
        // we fail loudly rather than silently dropping the claim, so
        // callers know the BIBLE path isn't actually active for them.
        if let Some(claim_raw) = args.get("evidence_claim") {
            let claim: sentinel_domain::evidence_adapter::EvidenceClaim =
                match serde_json::from_value(claim_raw.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        return McpToolResult::err(format!("Invalid 'evidence_claim' shape: {e}"));
                    }
                };
            let Some(registry) = self.evidence_adapters.as_ref() else {
                return McpToolResult::err(
                    "evidence_claim supplied but no evidence-adapter registry is wired \
                     on this handler — call McpHandler::with_evidence_adapters() at \
                     bootstrap, or omit evidence_claim",
                );
            };
            match registry.fetch(&claim).await {
                Ok(receipt) => {
                    if !receipt.verified {
                        return McpToolResult::err(format!(
                            "Evidence adapter '{}' returned verified=false for claim '{}'; \
                             refusing to seal step proof without third-party verification",
                            receipt.adapter_name, claim.claim_type
                        ));
                    }
                    // Fold the receipt into Evidence.custom under a
                    // well-known key so verifiers can locate it without
                    // walking arbitrary JSON. Single key = single source
                    // of truth; multiple receipts (cross-vendor) go in
                    // an array (#69 / future work).
                    let receipt_json = match serde_json::to_value(&receipt) {
                        Ok(v) => v,
                        Err(e) => {
                            return McpToolResult::err(format!("Receipt serialization error: {e}"));
                        }
                    };
                    // `Evidence.custom` is a `serde_json::Value`. The
                    // common case is `Null` (no custom data) or an object
                    // already. Promote `Null` to an object before inserting;
                    // refuse if it's a non-object scalar/array (caller
                    // mis-shaped the existing custom payload and we don't
                    // want to silently clobber it).
                    if evidence.custom.is_null() {
                        evidence.custom = serde_json::json!({});
                    }
                    let Some(custom_obj) = evidence.custom.as_object_mut() else {
                        return McpToolResult::err(
                            "evidence.custom is not an object — refusing to fold \
                             evidence_receipt into a non-object custom payload",
                        );
                    };
                    custom_obj.insert("evidence_receipt".to_string(), receipt_json);
                }
                Err(e) => {
                    return McpToolResult::err(format!(
                        "Evidence adapter could not fetch receipt for claim '{}': {e}",
                        claim.claim_type
                    ));
                }
            }
        }

        if args.get("judge_model").is_some() {
            return McpToolResult::err(
                "judge_model is workflow-configured authority and is not accepted from \
                 sentinel__submit_step_complete callers",
            );
        }

        let artifact = args
            .get("artifact")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // Account context: caller-provided wins; otherwise inherit from
        // the previous step in this skill's chain (M7.9 header/context
        // propagation, sentinel #58). Three cases for the arg:
        //
        //   * key absent             → inherit from prior step
        //   * key present, null      → explicit clear (no inheritance)
        //   * key present, string    → use that value
        //
        // This mirrors request-header semantics: missing header inherits
        // from upstream; explicitly empty header clears it.
        let account_context = match args.get("account_context") {
            None => {
                // Key absent → inherit from the most recent StepProof
                // in this skill's chain. Phase-only entries are skipped
                // (they don't carry the account dimension). If the
                // chain has no step entries yet, account_context = None.
                let s = self.state.read().await;
                s.proof_chain(skill).and_then(|chain| {
                    chain.entries.iter().rev().find_map(|e| match e {
                        sentinel_domain::proof::ProofEntry::Step(prev) => {
                            prev.account_context.clone()
                        }
                        _ => None,
                    })
                })
            }
            Some(v) if v.is_null() => None, // Explicit clear.
            Some(v) => v.as_str().map(std::string::ToString::to_string),
        };

        // Step verifier requirements (sentinel #71). Check each
        // requirement that matches these step coordinates against
        // the evidence we're about to seal. The check sees the
        // BIBLE-folded receipt (if any) because it runs AFTER the
        // BIBLE wireup section above. A failed verifier blocks
        // sealing with a clear error — the proof chain refuses to
        // record a downstream step on a missing or failed
        // third-party verification.
        for req in &self.step_verifiers {
            if req.matches(skill, phase_id, step_id) {
                if let Err(e) = req.check(&evidence.custom) {
                    return McpToolResult::err(format!(
                        "Step verifier requirement failed at {skill}/{phase_id}/{step_id}: {e}"
                    ));
                }
            }
        }

        let Some(workflow) = self
            .workflows
            .as_ref()
            .and_then(|workflows| workflows.get(skill))
        else {
            return McpToolResult::err(format!(
                "submit_step_complete requires configured LangGraph workflow context for skill '{skill}'"
            ));
        };
        let Some(phase_config) = workflow.phases.iter().find(|phase| phase.id == phase_id) else {
            return McpToolResult::err(format!(
                "Unknown phase '{phase_id}' for workflow '{skill}'. Cannot submit step completion."
            ));
        };
        let judge_model = phase_config.judge;
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
            .or_else(|| Some(step_description.to_string()));

        match self
            .proof_engine
            .submit_step_evidence_report(
                skill,
                phase_id,
                step_id,
                step_description,
                evidence,
                verdict,
                judge_model,
                artifact,
                account_context,
                started_at,
                workflow,
                summary,
            )
            .await
        {
            Ok(report) => {
                let proof = match serde_json::to_value(&report.proof) {
                    Ok(v) => v,
                    Err(e) => return McpToolResult::err(format!("Serialization error: {e}")),
                };
                let mut payload = proof.clone();
                let Some(obj) = payload.as_object_mut() else {
                    return McpToolResult::err(
                        "Serialization error: StepProof did not serialize to an object",
                    );
                };
                obj.insert("status".to_string(), serde_json::json!("accepted"));
                obj.insert("proof".to_string(), proof);
                let phase_graph = report.step_graph.graph_run.clone();
                obj.insert("phase_graph".to_string(), phase_graph.clone());
                obj.insert(
                    "workflow_authority".to_string(),
                    serde_json::json!("langgraph"),
                );
                if let Some(graph_state) = phase_graph.get("graph_state").cloned() {
                    obj.insert("graph_state".to_string(), graph_state);
                }
                if let Some(latest_checkpoint) = phase_graph.get("latest_checkpoint").cloned() {
                    obj.insert("latest_checkpoint".to_string(), latest_checkpoint);
                }
                McpToolResult::ok(payload)
            }
            Err(e) => McpToolResult::err(format!("submit_step_complete failed: {e}")),
        }
    }

    /// Query the proof corpus for historical chains matching a pattern (M4.3).
    ///
    /// **The moat tool** — what the router-as-planner (M7) reads from to
    /// decide which step combinations have worked in the past. No other
    /// agent system has this because no other agent system produces
    /// hash-verified execution chains in the first place.
    ///
    /// **Current scope (M4.3 v1)**: searches the *live* in-memory state
    /// across all skills in this session. Cross-session corpus aggregation
    /// (scanning `~/.claude/sentinel/proofs/` for archived chains from
    /// prior sessions) requires the persistence layer that doesn't exist
    /// yet — see follow-up task. The tool surface stays the same when
    /// cross-session lands; only the data source widens.
    ///
    /// Arguments:
    /// - `skill_filter` (optional string) — restrict to chains for this skill
    /// - `min_steps` (optional u64) — only return chains with at least N step entries
    /// - `successful_only` (optional bool, default true) — filter to chains where
    ///    every step has `judge_verdict.sufficient == true`
    /// - `max_results` (optional u64, default 50, capped at 500) — pagination cap
    ///
    /// Response shape:
    /// ```json
    /// {
    ///   "scope": "live-session",   // or "cross-session" once persistence lands
    ///   "total_matched": N,
    ///   "chains": [
    ///     {
    ///       "skill": "linear",
    ///       "session_id": "...",
    ///       "step_count": 3,
    ///       "phase_count": 0,
    ///       "all_sufficient": true,
    ///       "head_hash": "...",
    ///       "step_sequence": ["claim.1", "claim.2", "review.1"]  // pattern signal
    ///     },
    ///     ...
    ///   ]
    /// }
    /// ```
    ///
    /// The `step_sequence` field is the key signal: it lets the M7 router
    /// query "for prompts like X, what step-sequence patterns have worked
    /// before?" without dragging the full `StepProof` payloads across the
    /// MCP boundary.
    async fn query_proof_corpus(&self, args: serde_json::Value) -> McpToolResult {
        let skill_filter = args.get("skill_filter").and_then(|v| v.as_str());
        let min_steps = args
            .get("min_steps")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let successful_only = args
            .get("successful_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let max_results = args
            .get("max_results")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(50)
            .min(500) as usize;

        let state = self.state.read().await;

        // Iterate every chain in the live session. Filter and shape
        // into the response payload. Cross-session aggregation will
        // append additional sources here without changing the shape.
        let mut summaries: Vec<serde_json::Value> = Vec::new();
        let mut total_matched: u64 = 0;

        for (skill, chain) in state.proof_chains() {
            if let Some(want) = skill_filter {
                if skill != want {
                    continue;
                }
            }

            let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
                .entries
                .iter()
                .filter_map(|e| match e {
                    ProofEntry::Step(s) => Some(s),
                    _ => None,
                })
                .collect();

            if (step_entries.len() as u64) < min_steps {
                continue;
            }

            let all_sufficient = step_entries.iter().all(|s| s.judge_verdict.sufficient)
                && chain.phases_all_sufficient();
            if successful_only && !all_sufficient {
                continue;
            }

            let phase_count = chain.phase_count();

            // step_sequence: ordered "phase_id.step_id" coordinates. This
            // is the pattern signal the router queries against for
            // "which step combinations have worked before?"
            let step_sequence: Vec<String> = step_entries
                .iter()
                .map(|s| format!("{}.{}", s.phase_id, s.step_id))
                .collect();

            total_matched += 1;
            if summaries.len() < max_results {
                summaries.push(serde_json::json!({
                    "skill": chain.skill,
                    "session_id": chain.session_id,
                    "step_count": step_entries.len(),
                    "phase_count": phase_count,
                    "all_sufficient": all_sufficient,
                    "head_hash": chain.head_hash(),
                    "step_sequence": step_sequence,
                }));
            }
        }

        // Track which (session_id, skill) pairs already came from live
        // state. Live wins ties — a chain that's still in flight is the
        // current truth; the archived snapshot is a stale frame of that
        // same chain.
        let live_keys: std::collections::HashSet<(String, String)> = state
            .proof_chains()
            .map(|(_, c)| (c.session_id.clone(), c.skill.clone()))
            .collect();

        let Some(arch) = &self.archive else {
            return McpToolResult::err(
                "proof archive backing is not configured; query_proof_corpus requires cross-session archive access",
            );
        };

        // Cross-session: walk the archive index and merge historical chains
        // not already represented by live state.
        let mut scope = "live-session";
        let entries = crate::proof_archive::read_index(arch.fs.as_ref(), &arch.home);
        if !entries.is_empty() {
            scope = "cross-session";
        }
        for entry in entries {
            if live_keys.contains(&(entry.session_id.clone(), entry.skill.clone())) {
                continue; // Live wins — skip stale archive snapshot.
            }
            if let Some(want) = skill_filter {
                if entry.skill != want {
                    continue;
                }
            }
            if (entry.step_count as u64) < min_steps {
                continue;
            }
            if successful_only && !entry.all_sufficient {
                continue;
            }
            total_matched += 1;
            if summaries.len() < max_results {
                summaries.push(serde_json::json!({
                    "skill": entry.skill,
                    "session_id": entry.session_id,
                    "step_count": entry.step_count,
                    "phase_count": entry.phase_count,
                    "all_sufficient": entry.all_sufficient,
                    "head_hash": entry.head_hash,
                    "step_sequence": entry.step_sequence,
                    "archived_at": entry.archived_at,
                }));
            }
        }

        McpToolResult::ok(serde_json::json!({
            "scope": scope,
            "total_matched": total_matched,
            "chains": summaries,
        }))
    }
}

#[cfg(test)]
mod step_tools_tests {
    //! Tests for M4.1 step-level MCP tools. Drives the handler end-to-end
    //! against a real ProofEngine + state, asserts response shapes match
    //! what external MCP callers will rely on.

    use super::*;
    use crate::judge_service::JudgeService;
    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

    struct StubJudge;
    #[async_trait::async_trait]
    impl JudgeService for StubJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            unreachable!("step tools never call evaluate()")
        }
    }

    fn test_engine(state: Arc<RwLock<SessionState>>) -> Arc<ProofEngine> {
        Arc::new(
            ProofEngine::new(state, Arc::new(StubJudge))
                .with_signing(None, false)
                .with_test_step_graph_authority(),
        )
    }

    fn empty_archive_backing() -> ProofArchiveBacking {
        ProofArchiveBacking {
            home: std::path::PathBuf::from("/mock/home"),
            fs: Arc::new(crate::hooks::test_support::StubFs),
        }
    }

    fn registry_with_verified_adapter(
        adapter_name: &str,
        claim_types: &[&str],
    ) -> Arc<crate::evidence_adapters::EvidenceAdapterRegistry> {
        let mut registry = crate::evidence_adapters::EvidenceAdapterRegistry::new();
        registry.register(Box::new(
            crate::evidence_adapters::testing::StubAdapter::new(
                adapter_name,
                claim_types
                    .iter()
                    .map(|claim| (*claim).to_string())
                    .collect(),
                true,
                serde_json::json!({ "verified_by": adapter_name }),
            ),
        ));
        Arc::new(registry)
    }

    struct FixedSeverityLlm;

    #[async_trait::async_trait]
    impl sentinel_domain::ports::LlmPort for FixedSeverityLlm {
        async fn complete(
            &self,
            _request: sentinel_domain::ports::LlmRequest,
        ) -> std::result::Result<String, sentinel_domain::port_errors::LlmError> {
            Ok(r#"{"priority":2,"reasoning":"core workflow impact"}"#.to_string())
        }
    }

    struct TestPhaseGraphAuthority;

    #[async_trait::async_trait]
    impl crate::proof_engine::PhaseGraphAuthority for TestPhaseGraphAuthority {
        async fn apply_verdict(
            &self,
            skill: &str,
            session_id: &str,
            _workflow: &SkillWorkflow,
            phase_id: &str,
            _passed: bool,
        ) -> anyhow::Result<crate::proof_engine::PhaseGraphApplyResult> {
            let workflow_state = WorkflowState::new(skill, session_id);
            Ok(crate::proof_engine::PhaseGraphApplyResult {
                workflow_state,
                graph_run: serde_json::json!({
                    "workflow_authority": "langgraph",
                    "state": {
                        "skill": skill,
                        "session_id": session_id,
                        "current_phase": phase_id,
                    },
                    "latest_checkpoint": {
                        "thread_id": format!("sentinel.phase.{skill}.{session_id}"),
                        "checkpoint_id": "checkpoint-1",
                    },
                    "checkpoints": [],
                    "writes": [],
                    "topology": {
                        "graph": "phase",
                        "durable_checkpointer": true,
                    },
                }),
            })
        }
    }

    struct TestSeverityGraphAuditor;

    #[async_trait::async_trait]
    impl SeverityGraphAuditPort for TestSeverityGraphAuditor {
        async fn audit_severity_proposals(
            &self,
            proposals: &[SeverityProposal],
            graph_jsonl: &Path,
        ) -> anyhow::Result<SeverityGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut rows = Vec::new();
            let mut runs = Vec::new();
            let mut authorized_sets = 0usize;
            let mut skipped = 0usize;
            for proposal in proposals {
                let authorize = proposal.action == "set" && proposal.issue_id.is_some();
                let decision = if authorize { "set" } else { "skip" }.to_string();
                let checkpoint = authorize.then(|| {
                    format!(
                        "sentinel.decision.severity.{}#checkpoint-1",
                        proposal.identifier
                    )
                });
                authorized_sets += usize::from(authorize);
                skipped += usize::from(!authorize);
                let thread_id = format!("sentinel.decision.severity.{}", proposal.identifier);
                let run = serde_json::json!({
                    "thread_id": thread_id.clone(),
                    "state": {
                        "identifier": proposal.identifier.clone(),
                        "decision": decision.clone(),
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-1",
                        "thread_id": thread_id.clone(),
                        "state": {
                            "identifier": proposal.identifier.clone(),
                            "decision": decision.clone(),
                        },
                    }],
                    "write_history": [{
                        "checkpoint_id": "checkpoint-1",
                        "channel": "state",
                    }],
                    "stream": [{
                        "event_type": "custom",
                        "node_id": "classify",
                    }],
                    "topology": {
                        "graph": "severity",
                        "durable_checkpointer": true,
                    },
                });
                rows.push(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "graph": "severity",
                    "identifier": proposal.identifier.clone(),
                    "decision": decision.clone(),
                    "authorization_checkpoint": checkpoint.clone(),
                    "thread_id": thread_id.clone(),
                    "run": run.clone(),
                }));
                runs.push(SeverityGraphAuditRun {
                    identifier: proposal.identifier.clone(),
                    decision,
                    authorization_checkpoint: checkpoint,
                    thread_id,
                    run,
                });
            }
            let mut text = String::new();
            for row in rows {
                text.push_str(&serde_json::to_string(&row)?);
                text.push('\n');
            }
            std::fs::write(graph_jsonl, text)?;
            Ok(SeverityGraphAudit {
                workflow_authority: "langgraph",
                graph: "severity",
                graph_runs_path: graph_jsonl.to_path_buf(),
                proposals_audited: proposals.len(),
                authorized_sets,
                skipped,
                runs,
            })
        }
    }

    struct TestPmAuditGraphAuditor;

    #[async_trait::async_trait]
    impl PmAuditGraphAuditPort for TestPmAuditGraphAuditor {
        async fn audit_pm_flags(
            &self,
            flags: &[PmFlag],
            graph_jsonl: &Path,
        ) -> anyhow::Result<PmAuditGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut rows = Vec::new();
            let mut runs = Vec::new();
            let mut hard_violations = 0usize;
            let mut advisory_flags = 0usize;
            for flag in flags {
                let decision = match flag.category.as_str() {
                    "missing-estimate" | "oversized" | "blocked" | "no-milestone" => {
                        hard_violations += 1;
                        "hard-violation"
                    }
                    _ => {
                        advisory_flags += 1;
                        "advisory"
                    }
                };
                let thread_id = format!("sentinel.decision.pm_audit.{}", flag.identifier);
                let checkpoint = format!("{thread_id}#checkpoint-1");
                let run = serde_json::json!({
                    "thread_id": thread_id.clone(),
                    "state": {
                        "identifier": flag.identifier.clone(),
                        "category": flag.category.clone(),
                        "decision": decision,
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-1",
                        "thread_id": thread_id.clone(),
                        "state": {
                            "identifier": flag.identifier.clone(),
                            "category": flag.category.clone(),
                            "decision": decision,
                        },
                    }],
                    "write_history": [{
                        "checkpoint_id": "checkpoint-1",
                        "channel": "state",
                    }],
                    "stream": [{
                        "event_type": "custom",
                        "node_id": decision,
                    }],
                    "topology": {
                        "graph": "pm_audit",
                        "durable_checkpointer": true,
                    },
                });
                rows.push(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "graph": "pm_audit",
                    "identifier": flag.identifier.clone(),
                    "category": flag.category.clone(),
                    "decision": decision,
                    "authorization_checkpoint": checkpoint.clone(),
                    "thread_id": thread_id.clone(),
                    "run": run.clone(),
                }));
                runs.push(PmAuditGraphAuditRun {
                    identifier: flag.identifier.clone(),
                    category: flag.category.clone(),
                    decision: decision.to_string(),
                    authorization_checkpoint: Some(checkpoint),
                    thread_id,
                    run,
                });
            }
            let mut text = String::new();
            for row in rows {
                text.push_str(&serde_json::to_string(&row)?);
                text.push('\n');
            }
            std::fs::write(graph_jsonl, text)?;
            Ok(PmAuditGraphAudit {
                workflow_authority: "langgraph",
                graph: "pm_audit",
                graph_runs_path: graph_jsonl.to_path_buf(),
                flags_audited: flags.len(),
                hard_violations,
                advisory_flags,
                cleared: 0,
                runs,
            })
        }
    }

    struct TestLinearHealthGraphAuditor;

    #[async_trait::async_trait]
    impl LinearHealthGraphAuditPort for TestLinearHealthGraphAuditor {
        async fn audit_linear_health(
            &self,
            summary: &HealthSummary,
            graph_jsonl: &Path,
        ) -> anyhow::Result<LinearHealthGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let decision = summary.grade.clone();
            let thread_id = "sentinel.decision.linear_health.board".to_string();
            let checkpoint = format!("{thread_id}#checkpoint-1");
            let run = serde_json::json!({
                "thread_id": thread_id.clone(),
                "state": {
                    "total_score": summary.total_score,
                    "grade": summary.grade.clone(),
                    "decision": decision.clone(),
                },
                "checkpoints": [{
                    "checkpoint_id": "checkpoint-1",
                    "thread_id": thread_id.clone(),
                    "state": {
                        "total_score": summary.total_score,
                        "grade": summary.grade.clone(),
                        "decision": decision.clone(),
                    },
                }],
                "write_history": [{
                    "checkpoint_id": "checkpoint-1",
                    "channel": "state",
                }],
                "stream": [{
                    "event_type": "custom",
                    "node_id": decision.clone(),
                }],
                "topology": {
                    "graph": "linear_health",
                    "durable_checkpointer": true,
                },
            });
            let row = serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "linear_health",
                "decision": decision.clone(),
                "authorization_checkpoint": checkpoint.clone(),
                "thread_id": thread_id.clone(),
                "run": run.clone(),
            });
            std::fs::write(graph_jsonl, format!("{}\n", serde_json::to_string(&row)?))?;
            Ok(LinearHealthGraphAudit {
                workflow_authority: "langgraph",
                graph: "linear_health",
                graph_runs_path: graph_jsonl.to_path_buf(),
                decision,
                authorization_checkpoint: Some(checkpoint),
                thread_id,
                run,
            })
        }
    }

    struct TestDevScorecardGraphAuditor;

    #[async_trait::async_trait]
    impl DevScorecardGraphAuditPort for TestDevScorecardGraphAuditor {
        async fn audit_dev_scores(
            &self,
            scores: &[DevScore],
            graph_jsonl: &Path,
        ) -> anyhow::Result<DevScorecardGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut rows = Vec::new();
            let mut runs = Vec::new();
            let mut attribution_divergences = 0usize;
            let mut excellent = 0usize;
            let mut healthy = 0usize;
            let mut needs_attention = 0usize;
            for score in scores {
                let decision = if score.attribution_divergence {
                    attribution_divergences += 1;
                    "attribution-divergence"
                } else if score.score >= 85.0 {
                    excellent += 1;
                    "excellent"
                } else if score.score >= 70.0 {
                    healthy += 1;
                    "healthy"
                } else {
                    needs_attention += 1;
                    "needs-attention"
                };
                let thread_id = format!("sentinel.decision.dev_scorecard.{}", score.name);
                let checkpoint = format!("{thread_id}#checkpoint-1");
                let run = serde_json::json!({
                    "thread_id": thread_id.clone(),
                    "state": {
                        "identifier": score.name.clone(),
                        "score": score.score,
                        "attribution_divergence": score.attribution_divergence,
                        "decision": decision,
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-1",
                        "thread_id": thread_id.clone(),
                        "state": {
                            "identifier": score.name.clone(),
                            "score": score.score,
                            "decision": decision,
                        },
                    }],
                    "write_history": [{
                        "checkpoint_id": "checkpoint-1",
                        "channel": "state",
                    }],
                    "stream": [{
                        "event_type": "custom",
                        "node_id": decision,
                    }],
                    "topology": {
                        "graph": "dev_scorecard",
                        "durable_checkpointer": true,
                    },
                });
                rows.push(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "graph": "dev_scorecard",
                    "identifier": score.name.clone(),
                    "decision": decision,
                    "authorization_checkpoint": checkpoint.clone(),
                    "thread_id": thread_id.clone(),
                    "run": run.clone(),
                }));
                runs.push(DevScorecardGraphAuditRun {
                    identifier: score.name.clone(),
                    decision: decision.to_string(),
                    authorization_checkpoint: Some(checkpoint),
                    thread_id,
                    run,
                });
            }
            let mut text = String::new();
            for row in rows {
                text.push_str(&serde_json::to_string(&row)?);
                text.push('\n');
            }
            std::fs::write(graph_jsonl, text)?;
            Ok(DevScorecardGraphAudit {
                workflow_authority: "langgraph",
                graph: "dev_scorecard",
                graph_runs_path: graph_jsonl.to_path_buf(),
                devs_audited: scores.len(),
                attribution_divergences,
                excellent,
                healthy,
                needs_attention,
                runs,
            })
        }
    }

    struct TestTokenCostGraphAuditor;

    #[async_trait::async_trait]
    impl TokenCostGraphAuditPort for TestTokenCostGraphAuditor {
        async fn audit_token_cost(
            &self,
            summary: &TokenCostSummary,
            graph_jsonl: &Path,
        ) -> anyhow::Result<TokenCostGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let decision = if summary.tickets == 0 || summary.total_tokens == 0 {
                "no-data"
            } else if summary.unknown_model_tokens > 0 {
                "unknown-model-risk"
            } else if summary.cache_savings_usd > 0.0 {
                "cache-effective"
            } else {
                "no-savings"
            }
            .to_string();
            let thread_id = "sentinel.decision.token_cost.aggregate".to_string();
            let checkpoint = format!("{thread_id}#checkpoint-1");
            let run = serde_json::json!({
                "thread_id": thread_id.clone(),
                "state": {
                    "identifier": "aggregate",
                    "tickets": summary.tickets,
                    "total_tokens": summary.total_tokens,
                    "decision": decision.clone(),
                },
                "checkpoints": [{
                    "checkpoint_id": "checkpoint-1",
                    "thread_id": thread_id.clone(),
                    "state": {
                        "identifier": "aggregate",
                        "decision": decision.clone(),
                    },
                }],
                "write_history": [{
                    "checkpoint_id": "checkpoint-1",
                    "channel": "state",
                }],
                "stream": [{
                    "event_type": "custom",
                    "node_id": decision.clone(),
                }],
                "topology": {
                    "graph": "token_cost",
                    "durable_checkpointer": true,
                },
            });
            let row = serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "token_cost",
                "decision": decision.clone(),
                "authorization_checkpoint": checkpoint.clone(),
                "thread_id": thread_id.clone(),
                "run": run.clone(),
            });
            std::fs::write(graph_jsonl, format!("{}\n", serde_json::to_string(&row)?))?;
            Ok(TokenCostGraphAudit {
                workflow_authority: "langgraph",
                graph: "token_cost",
                graph_runs_path: graph_jsonl.to_path_buf(),
                decision,
                authorization_checkpoint: Some(checkpoint),
                thread_id,
                run,
            })
        }
    }

    struct TestTokenUsageGraphAuditor;

    #[async_trait::async_trait]
    impl TokenUsageGraphAuditPort for TestTokenUsageGraphAuditor {
        async fn audit_token_usage(
            &self,
            report: &ScanReport,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let decision = if report.total_sessions == 0 {
                "no-data"
            } else if report.unpriced_tokens > 0 {
                "unpriced-model-risk"
            } else if report.unmapped_sessions > 0 {
                "mapping-risk"
            } else {
                "healthy-usage"
            }
            .to_string();
            let thread_id = "sentinel.decision.token_usage.aggregate".to_string();
            let checkpoint = format!("{thread_id}#checkpoint-1");
            let run = serde_json::json!({
                "thread_id": thread_id.clone(),
                "state": {
                    "total_sessions": report.total_sessions,
                    "mapped_sessions": report.mapped_sessions,
                    "unmapped_sessions": report.unmapped_sessions,
                    "unpriced_sessions": report.unpriced_sessions,
                    "unpriced_tokens": report.unpriced_tokens,
                    "decision": decision.clone(),
                },
                "checkpoints": [{
                    "checkpoint_id": "checkpoint-1",
                    "thread_id": thread_id.clone(),
                    "state": {
                        "decision": decision.clone(),
                    },
                }],
                "write_history": [{
                    "checkpoint_id": "checkpoint-1",
                    "channel": "state",
                }],
                "stream": [{
                    "event_type": "custom",
                    "node_id": decision.clone(),
                }],
                "topology": {
                    "graph": "token_usage",
                    "durable_checkpointer": true,
                },
            });
            let row = serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "token_usage",
                "decision": decision.clone(),
                "authorization_checkpoint": checkpoint.clone(),
                "thread_id": thread_id.clone(),
                "run": run.clone(),
            });
            std::fs::write(graph_jsonl, format!("{}\n", serde_json::to_string(&row)?))?;
            Ok(AggregateGraphAudit {
                workflow_authority: "langgraph",
                graph: "token_usage",
                graph_runs_path: graph_jsonl.to_path_buf(),
                decision,
                authorization_checkpoint: Some(checkpoint),
                thread_id,
                run,
            })
        }
    }

    struct TestAggregateGraphAuditor;

    fn aggregate_audit_for(graph: &'static str, graph_jsonl: &Path) -> AggregateGraphAudit {
        let thread_id = format!("sentinel.decision.{graph}.aggregate");
        let checkpoint = format!("{thread_id}#checkpoint-1");
        AggregateGraphAudit {
            workflow_authority: "langgraph",
            graph,
            graph_runs_path: graph_jsonl.to_path_buf(),
            decision: "validated".to_string(),
            authorization_checkpoint: Some(checkpoint),
            thread_id: thread_id.clone(),
            run: serde_json::json!({
                "thread_id": thread_id,
                "state": {"decision": "validated"},
                "checkpoints": [{
                    "checkpoint_id": "checkpoint-1",
                    "thread_id": thread_id,
                }],
                "write_history": [{
                    "checkpoint_id": "checkpoint-1",
                    "channel": "state",
                }],
                "topology": {
                    "graph": graph,
                    "durable_checkpointer": true,
                },
            }),
        }
    }

    #[async_trait::async_trait]
    impl CacheEfficiencyGraphAuditPort for TestAggregateGraphAuditor {
        async fn audit_cache_efficiency(
            &self,
            _report: &CacheReport,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            Ok(aggregate_audit_for("cache_efficiency", graph_jsonl))
        }
    }

    #[async_trait::async_trait]
    impl CostPerPointGraphAuditPort for TestAggregateGraphAuditor {
        async fn audit_cost_per_point(
            &self,
            _report: &CostPerPointReport,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            Ok(aggregate_audit_for("cost_per_point", graph_jsonl))
        }
    }

    #[async_trait::async_trait]
    impl DeployFrequencyGraphAuditPort for TestAggregateGraphAuditor {
        async fn audit_deploy_frequency(
            &self,
            _summary: &DeploySummary,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            Ok(aggregate_audit_for("deploy_frequency", graph_jsonl))
        }
    }

    #[async_trait::async_trait]
    impl PrReviewGraphAuditPort for TestAggregateGraphAuditor {
        async fn audit_pr_review(
            &self,
            _report: &PrReviewReport,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            Ok(aggregate_audit_for("pr_review", graph_jsonl))
        }
    }

    #[async_trait::async_trait]
    impl RoiGraphAuditPort for TestAggregateGraphAuditor {
        async fn audit_roi(
            &self,
            _report: &RoiReport,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            Ok(aggregate_audit_for("roi", graph_jsonl))
        }
    }

    #[async_trait::async_trait]
    impl SlaGraphAuditPort for TestAggregateGraphAuditor {
        async fn audit_sla(
            &self,
            _summary: &BreachesSummary,
            graph_jsonl: &Path,
        ) -> anyhow::Result<AggregateGraphAudit> {
            Ok(aggregate_audit_for("sla", graph_jsonl))
        }
    }

    struct TestEvalRunGraphRunner;

    #[async_trait::async_trait]
    impl EvalRunGraphPort for TestEvalRunGraphRunner {
        async fn run_eval(&self, request: EvalRunRequest) -> anyhow::Result<EvalRunGraphAudit> {
            let run_id = sentinel_domain::eval::EvalRunId::new(request.run_id)?;
            let case_id = sentinel_domain::eval::EvalCaseId::new(
                request
                    .case_ids
                    .first()
                    .map(String::as_str)
                    .unwrap_or("case-1"),
            )?;
            let rubric = sentinel_domain::eval::ScoringRubric::ba_default();
            let axis_scores = vec![
                sentinel_domain::eval::EvalAxisScore::new(
                    sentinel_domain::eval::EvalAxis::CitationDensityAccuracy,
                    0.90,
                    1.0,
                ),
                sentinel_domain::eval::EvalAxisScore::new(
                    sentinel_domain::eval::EvalAxis::RequirementsCoverage,
                    0.90,
                    1.0,
                ),
                sentinel_domain::eval::EvalAxisScore::new(
                    sentinel_domain::eval::EvalAxis::AlternativesSeriousness,
                    0.90,
                    1.0,
                ),
                sentinel_domain::eval::EvalAxisScore::new(
                    sentinel_domain::eval::EvalAxis::TonalCalibration,
                    0.90,
                    1.0,
                ),
                sentinel_domain::eval::EvalAxisScore::new(
                    sentinel_domain::eval::EvalAxis::OutcomeRealism,
                    0.90,
                    2.0,
                ),
                sentinel_domain::eval::EvalAxisScore::new(
                    sentinel_domain::eval::EvalAxis::StakeholderFit,
                    0.90,
                    1.0,
                ),
            ];
            let score = sentinel_domain::eval::EvalScore::new(
                case_id.clone(),
                run_id.clone(),
                axis_scores,
                &rubric,
            );
            let run = sentinel_domain::eval::EvalRunResult {
                run_id: run_id.clone(),
                started_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
                completed_at: Utc.timestamp_opt(1_700_000_010, 0).unwrap(),
                case_results: vec![sentinel_domain::eval::EvalCaseResult {
                    case_id,
                    run_id: run_id.clone(),
                    candidate_output: format!(
                        "candidate artifact from {}",
                        request.candidates_path.display()
                    ),
                    score: Some(score),
                    timing_ms: 10,
                    completed_at: Utc.timestamp_opt(1_700_000_005, 0).unwrap(),
                    error: None,
                }],
            };
            let graph_jsonl = request
                .runs_dir
                .ok_or_else(|| anyhow::anyhow!("test eval graph runner requires runs_dir"))?
                .join(format!("{}.graph-runs.jsonl", run_id.as_str()));
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let thread_id = format!("sentinel.decision.eval.{}", run_id.as_str());
            let checkpoint = format!("{thread_id}#checkpoint-1");
            let run_json = serde_json::json!({
                "thread_id": thread_id.clone(),
                "state": {
                    "identifier": run_id.as_str(),
                    "case_count": 1,
                    "decision": "strong",
                },
                "checkpoints": [{
                    "checkpoint_id": "checkpoint-1",
                    "thread_id": thread_id.clone(),
                    "state": {
                        "identifier": run_id.as_str(),
                        "decision": "strong",
                    },
                }],
                "write_history": [{
                    "checkpoint_id": "checkpoint-1",
                    "channel": "state",
                }],
                "stream": [{
                    "event_type": "custom",
                    "node_id": "strong",
                }],
                "topology": {
                    "graph": "eval",
                    "durable_checkpointer": true,
                },
            });
            let row = serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "eval",
                "decision": "strong",
                "authorization_checkpoint": checkpoint.clone(),
                "thread_id": thread_id.clone(),
                "run": run_json.clone(),
            });
            std::fs::write(&graph_jsonl, format!("{}\n", serde_json::to_string(&row)?))?;
            Ok(EvalRunGraphAudit {
                workflow_authority: "langgraph",
                run,
                graph_audit: AggregateGraphAudit {
                    workflow_authority: "langgraph",
                    graph: "eval",
                    graph_runs_path: graph_jsonl,
                    decision: "strong".to_string(),
                    authorization_checkpoint: Some(checkpoint),
                    thread_id,
                    run: run_json,
                },
            })
        }
    }

    struct TestBaDraftGraphRunner;

    #[async_trait::async_trait]
    impl BaDraftGraphPort for TestBaDraftGraphRunner {
        async fn draft_ba_recommendation(
            &self,
            request: BaDraftGraphRequest,
        ) -> anyhow::Result<BaDraftGraphRun> {
            let ts = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
            let recommendation_id =
                sentinel_domain::ba::RecommendationId::new("mcp-ba-draft").unwrap();
            let recommendation = sentinel_domain::ba::BaRecommendation {
                recommendation_id: recommendation_id.clone(),
                brief: request.brief,
                stakeholder_audience: sentinel_domain::ba::StakeholderAudience::Exec,
                body: format!(
                    "Recommendation for {} with {} constraints.",
                    request.agent_id,
                    request.constraints.len()
                ),
                citations: vec![sentinel_domain::ba::ArtifactReference {
                    artifact_id: "linear://issue/FPCRM-42".to_string(),
                    content_hash: "hash-1".to_string(),
                    provenance_class: sentinel_domain::ba::ProvenanceClass::SystemOfRecord,
                    retrieved_at: ts,
                }],
                requirement_refs: vec![sentinel_domain::ba::RequirementRef {
                    orchestration_id: "orch-1".to_string(),
                    matrix_row_id: "row-1".to_string(),
                    content_hash: "req-hash".to_string(),
                    statement: "stakeholder wants growth".to_string(),
                }],
                spec_challenge: sentinel_domain::spec_challenge::SpecChallenge {
                    work_id: sentinel_domain::spec_challenge::WorkId::new("work-1").unwrap(),
                    agent_id: request.agent_id,
                    challenged_spec: sentinel_domain::spec_challenge::SpecReference {
                        hash: "brief-hash".to_string(),
                        source: "brief".to_string(),
                    },
                    reversibility_class:
                        sentinel_domain::reversibility::ReversibilityClass::Catastrophic,
                    assumptions: sentinel_domain::spec_challenge::ChallengeCategory::new(vec![
                        sentinel_domain::spec_challenge::Assumption {
                            statement: "growth matters".to_string(),
                            confidence:
                                sentinel_domain::spec_challenge::AssumptionConfidence::Medium,
                            blast_if_wrong:
                                sentinel_domain::reversibility::ReversibilityClass::Irreversible,
                        },
                    ]),
                    gaps: sentinel_domain::spec_challenge::ChallengeCategory::new(vec![
                        sentinel_domain::spec_challenge::SpecGap {
                            topic: "budget".to_string(),
                            how_resolved:
                                sentinel_domain::spec_challenge::GapResolution::OperatorClarified,
                            inference_source: None,
                        },
                    ]),
                    ambiguities: sentinel_domain::spec_challenge::ChallengeCategory::new(vec![
                        sentinel_domain::spec_challenge::Ambiguity {
                            spec_excerpt: "scale".to_string(),
                            interpretations: vec!["users".to_string(), "revenue".to_string()],
                            chosen: "users".to_string(),
                            rationale: "brief context".to_string(),
                        },
                    ]),
                    alternatives_considered:
                        sentinel_domain::spec_challenge::ChallengeCategory::new(vec![
                            sentinel_domain::spec_challenge::Alternative {
                                description: "vertical scaling".to_string(),
                                why_rejected: "cost".to_string(),
                            },
                        ]),
                    constraints_not_satisfied:
                        sentinel_domain::spec_challenge::ChallengeCategory::none("all met"),
                    created_at: ts,
                },
                generated_at: ts,
                agent_id: "ba-orchestrator".to_string(),
            };
            let graph_jsonl = mcp_ba_draft_graph_jsonl()?;
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let thread_id = "sentinel.decision.ba_draft.mcp-ba-draft".to_string();
            let checkpoint = format!("{thread_id}#checkpoint-1");
            let run = serde_json::json!({
                "thread_id": thread_id.clone(),
                "state": {
                    "identifier": recommendation_id.as_str(),
                    "audience": request.audience,
                    "decision": "high-risk-ready",
                },
                "checkpoints": [{
                    "checkpoint_id": "checkpoint-1",
                    "thread_id": thread_id.clone(),
                    "state": {
                        "identifier": recommendation_id.as_str(),
                        "decision": "high-risk-ready",
                    },
                }],
                "write_history": [{
                    "checkpoint_id": "checkpoint-1",
                    "channel": "state",
                }],
                "stream": [{
                    "event_type": "custom",
                    "node_id": "high_risk_ready",
                }],
                "topology": {
                    "graph": "ba_draft",
                    "durable_checkpointer": true,
                },
            });
            let row = serde_json::json!({
                "workflow_authority": "langgraph",
                "graph": "ba_draft",
                "decision": "high-risk-ready",
                "authorization_checkpoint": checkpoint.clone(),
                "thread_id": thread_id.clone(),
                "run": run.clone(),
            });
            std::fs::write(&graph_jsonl, format!("{}\n", serde_json::to_string(&row)?))?;
            Ok(BaDraftGraphRun {
                workflow_authority: "langgraph",
                recommendation,
                graph_audit: AggregateGraphAudit {
                    workflow_authority: "langgraph",
                    graph: "ba_draft",
                    graph_runs_path: graph_jsonl,
                    decision: "high-risk-ready".to_string(),
                    authorization_checkpoint: Some(checkpoint),
                    thread_id,
                    run,
                },
            })
        }
    }

    struct TestCodeReconciliationAuditor;

    #[async_trait::async_trait]
    impl CodeReconciliationAuditPort for TestCodeReconciliationAuditor {
        async fn audit_code_flags(
            &self,
            flags: &[CodeFlag],
            graph_jsonl: &Path,
        ) -> anyhow::Result<CodeReconciliationGraphAudit> {
            if let Some(parent) = graph_jsonl.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut rows = Vec::new();
            let mut runs = Vec::new();
            for flag in flags {
                let thread_id = format!("sentinel.decision.reconciliation.{}", flag.identifier);
                let checkpoint = format!("{thread_id}#checkpoint-1");
                let run = serde_json::json!({
                    "thread_id": thread_id,
                    "state": {
                        "identifier": flag.identifier,
                        "decision": "Flag",
                        "verdict": "Reverted",
                    },
                    "checkpoints": [{
                        "checkpoint_id": "checkpoint-1",
                        "thread_id": thread_id,
                        "state": {"identifier": flag.identifier, "decision": "Flag"},
                    }],
                    "write_history": [{
                        "checkpoint_id": "checkpoint-1",
                        "channel": "state",
                    }],
                    "stream": [{
                        "event_type": "custom",
                        "node_id": "flag",
                    }],
                    "topology": {
                        "graph": "reconciliation",
                        "durable_checkpointer": true,
                    },
                });
                rows.push(serde_json::json!({
                    "workflow_authority": "langgraph",
                    "graph": "reconciliation",
                    "identifier": flag.identifier,
                    "decision": "flag",
                    "authorization_checkpoint": checkpoint,
                    "thread_id": thread_id,
                    "run": run,
                }));
                runs.push(CodeReconciliationGraphAuditRun {
                    identifier: flag.identifier.clone(),
                    decision: "flag".to_string(),
                    authorization_checkpoint: Some(checkpoint),
                    thread_id,
                    run,
                });
            }
            let mut text = String::new();
            for row in rows {
                text.push_str(&serde_json::to_string(&row)?);
                text.push('\n');
            }
            std::fs::write(graph_jsonl, text)?;
            Ok(CodeReconciliationGraphAudit {
                workflow_authority: "langgraph",
                graph: "reconciliation",
                graph_runs_path: graph_jsonl.to_path_buf(),
                flags_audited: flags.len(),
                authorized_flags: flags.len(),
                cleared: 0,
                runs,
            })
        }
    }

    struct TestMcpProofReadGraphAuditor;

    #[async_trait::async_trait]
    impl McpProofReadGraphAuditPort for TestMcpProofReadGraphAuditor {
        async fn audit_mcp_proof_read(
            &self,
            surface: McpProofReadSurface,
            response: &serde_json::Value,
            graph_jsonl: &Path,
        ) -> anyhow::Result<McpProofReadGraphAudit> {
            use sha2::Digest as _;

            let response_sha256 = hex::encode(sha2::Sha256::digest(
                serde_json::to_vec(response).unwrap_or_default(),
            ));
            let skill = response
                .get("skill")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let thread_id = format!("sentinel.mcp_proof_read.{}.{}", surface.label(), skill);
            let checkpoint = format!("{thread_id}#checkpoint-1");
            Ok(McpProofReadGraphAudit {
                workflow_authority: "langgraph",
                graph: "mcp_proof_read",
                surface: surface.label(),
                graph_runs_path: graph_jsonl.to_path_buf(),
                response_sha256,
                decision: "verified".to_string(),
                authorization_checkpoint: checkpoint,
                thread_id,
                run: serde_json::json!({
                    "surface": surface.label(),
                    "topology": {
                        "graph": "mcp_proof_read",
                        "durable_checkpointer": true,
                    },
                }),
            })
        }
    }

    fn enterprise_test_engine(state: Arc<RwLock<SessionState>>) -> Arc<ProofEngine> {
        Arc::new(
            ProofEngine::new(state, Arc::new(StubJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(TestPhaseGraphAuthority))
                .with_test_step_graph_authority(),
        )
    }

    fn fully_wired_enterprise_test_handler() -> McpHandler {
        let state = Arc::new(RwLock::new(SessionState::new(
            "enterprise-runtime-validation",
        )));
        let proof_engine = enterprise_test_engine(state.clone());
        McpHandler::new(state, proof_engine)
            .with_default_test_workflows()
            .with_archive(empty_archive_backing())
            .with_llm(Arc::new(FixedSeverityLlm))
            .with_severity_graph_auditor(Arc::new(TestSeverityGraphAuditor))
            .with_pm_audit_graph_auditor(Arc::new(TestPmAuditGraphAuditor))
            .with_linear_health_graph_auditor(Arc::new(TestLinearHealthGraphAuditor))
            .with_dev_scorecard_graph_auditor(Arc::new(TestDevScorecardGraphAuditor))
            .with_token_cost_graph_auditor(Arc::new(TestTokenCostGraphAuditor))
            .with_token_usage_graph_auditor(Arc::new(TestTokenUsageGraphAuditor))
            .with_cache_efficiency_graph_auditor(Arc::new(TestAggregateGraphAuditor))
            .with_cost_per_point_graph_auditor(Arc::new(TestAggregateGraphAuditor))
            .with_deploy_frequency_graph_auditor(Arc::new(TestAggregateGraphAuditor))
            .with_pr_review_graph_auditor(Arc::new(TestAggregateGraphAuditor))
            .with_roi_graph_auditor(Arc::new(TestAggregateGraphAuditor))
            .with_sla_graph_auditor(Arc::new(TestAggregateGraphAuditor))
            .with_code_reconciliation_auditor(Arc::new(TestCodeReconciliationAuditor))
            .with_mcp_proof_read_graph_auditor(Arc::new(TestMcpProofReadGraphAuditor))
            .with_eval_runner(Arc::new(TestEvalRunGraphRunner))
            .with_ba_draft_runner(Arc::new(TestBaDraftGraphRunner))
    }

    #[test]
    fn enterprise_langgraph_runtime_validation_rejects_partial_handler() {
        let state = Arc::new(RwLock::new(SessionState::new("partial-enterprise-runtime")));
        let handler = McpHandler::new(state.clone(), test_engine(state));

        let err = handler
            .validate_enterprise_langgraph_runtime()
            .expect_err("partial MCP runtime must fail startup validation")
            .to_string();

        assert!(err.contains("proof_engine.phase_graph_authority"));
        assert!(err.contains("workflow_catalog"));
        assert!(err.contains("llm_port"));
        assert!(err.contains("severity_graph_auditor"));
        assert!(err.contains("ba_draft_runner"));
    }

    #[test]
    fn enterprise_langgraph_runtime_validation_accepts_fully_wired_handler() {
        fully_wired_enterprise_test_handler()
            .validate_enterprise_langgraph_runtime()
            .expect("fully wired enterprise MCP runtime should pass startup validation");
    }

    struct HomeGuard {
        previous_home: Option<std::ffi::OsString>,
        previous_userprofile: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(path: &Path) -> Self {
            let previous_home = std::env::var_os("HOME");
            let previous_userprofile = std::env::var_os("USERPROFILE");
            std::env::set_var("HOME", path);
            std::env::set_var("USERPROFILE", path);
            Self {
                previous_home,
                previous_userprofile,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.previous_userprofile.take() {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn write_linear_cache(home: &Path) {
        let sentinel_dir = home.join(".claude").join("sentinel");
        std::fs::create_dir_all(&sentinel_dir).expect("sentinel dir");
        std::fs::write(
            sentinel_dir.join("linear-assigned.json"),
            serde_json::to_vec(&serde_json::json!([{
                "id": "linear-issue-id",
                "identifier": "FPCRM-777",
                "title": "Core workflow broken",
                "description": "Users cannot complete the main workflow.",
                "priority": 0
            }]))
            .expect("cache json"),
        )
        .expect("linear cache");
    }

    fn write_pm_audit_inputs(home: &Path) {
        let sentinel_dir = home.join(".claude").join("sentinel");
        std::fs::create_dir_all(&sentinel_dir).expect("sentinel dir");
        std::fs::write(
            sentinel_dir.join("linear-assigned.json"),
            serde_json::to_vec(&serde_json::json!([
                {
                    "identifier": "FPCRM-901",
                    "title": "Oversized unstarted work",
                    "estimate": 8,
                    "state": {"name": "Backlog", "type": "backlog"}
                },
                {
                    "identifier": "FPCRM-902",
                    "title": "QA bounced",
                    "estimate": 3,
                    "state": {"name": "QA Failed", "type": "started"}
                }
            ]))
            .expect("linear cache json"),
        )
        .expect("linear cache");
    }

    fn write_linear_health_inputs(home: &Path) {
        let sentinel_dir = home.join(".claude").join("sentinel");
        std::fs::create_dir_all(&sentinel_dir).expect("sentinel dir");
        std::fs::write(
            sentinel_dir.join("linear-assigned.json"),
            serde_json::to_vec(&serde_json::json!([
                {
                    "identifier": "FPCRM-911",
                    "estimate": 3,
                    "state": {"name": "Backlog", "type": "backlog"}
                },
                {
                    "identifier": "FPCRM-912",
                    "estimate": 5,
                    "state": {"name": "Done", "type": "completed"}
                }
            ]))
            .expect("linear health cache json"),
        )
        .expect("linear health cache");
    }

    fn write_dev_scorecard_inputs(home: &Path) {
        let sentinel_dir = home.join(".claude").join("sentinel");
        std::fs::create_dir_all(&sentinel_dir).expect("sentinel dir");
        std::fs::write(
            sentinel_dir.join("dev-git-stats.json"),
            serde_json::to_vec(&serde_json::json!({
                "devs": [
                    {
                        "name": "Rene",
                        "commits": 90,
                        "active_days": 15,
                        "merged_prs": 20,
                        "delivered_tickets": [
                            "FPCRM-921",
                            "FPCRM-922",
                            "FPCRM-923",
                            "FPCRM-924",
                            "FPCRM-925"
                        ],
                        "linear_assignee_completed": 0
                    },
                    {
                        "name": "Ada",
                        "commits": 45,
                        "active_days": 10,
                        "merged_prs": 9,
                        "delivered_tickets": ["FPCRM-926"],
                        "linear_assignee_completed": 1
                    }
                ]
            }))
            .expect("git stats json"),
        )
        .expect("git stats");
        std::fs::write(
            sentinel_dir.join("linear-assigned.json"),
            serde_json::to_vec(&serde_json::json!([
                {"identifier": "FPCRM-921", "state": {"name": "Done", "type": "completed"}},
                {"identifier": "FPCRM-922", "state": {"name": "Done", "type": "completed"}},
                {"identifier": "FPCRM-923", "state": {"name": "Done", "type": "completed"}},
                {"identifier": "FPCRM-924", "state": {"name": "Done", "type": "completed"}},
                {"identifier": "FPCRM-925", "state": {"name": "Done", "type": "completed"}},
                {"identifier": "FPCRM-926", "state": {"name": "Done", "type": "completed"}}
            ]))
            .expect("linear cache json"),
        )
        .expect("linear cache");
    }

    fn write_token_cost_inputs(home: &Path) {
        let metrics_dir = home.join(".claude").join("sentinel").join("metrics");
        std::fs::create_dir_all(&metrics_dir).expect("metrics dir");
        let rows = [
            serde_json::json!({
                "total_input": 500_000,
                "cache_read": 1_000_000,
                "cache_creation": 100_000,
                "output": 100_000,
                "models": {"claude-opus-4-8": 3}
            }),
            serde_json::json!({
                "total_input": 250_000,
                "cache_read": 500_000,
                "cache_creation": 50_000,
                "output": 50_000,
                "models": {"claude-opus-4-8": 2}
            }),
        ];
        let mut text = String::new();
        for row in rows {
            text.push_str(&serde_json::to_string(&row).expect("token row json"));
            text.push('\n');
        }
        std::fs::write(metrics_dir.join("tokens-per-ticket.jsonl"), text).expect("token rows");
    }

    fn write_code_audit_inputs(home: &Path) {
        let sentinel_dir = home.join(".claude").join("sentinel");
        std::fs::create_dir_all(&sentinel_dir).expect("sentinel dir");
        std::fs::write(
            sentinel_dir.join("linear-assigned.json"),
            serde_json::to_vec(&serde_json::json!([{
                "identifier": "FPCRM-888",
                "state": {"name": "Completed", "type": "completed"}
            }]))
            .expect("linear cache json"),
        )
        .expect("linear cache");
        std::fs::write(
            sentinel_dir.join("ticket-code-evidence.json"),
            serde_json::to_vec(&serde_json::json!({})).expect("evidence json"),
        )
        .expect("evidence map");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn linear_pm_audit_requires_langgraph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("pm-audit-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__linear_pm_audit".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("PM audit LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn linear_pm_audit_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        write_pm_audit_inputs(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("pm-audit-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_pm_audit_graph_auditor(Arc::new(TestPmAuditGraphAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__linear_pm_audit".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "PM audit failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["issues_total"], 2);
        assert_eq!(content["summary"]["oversized_open"], 1);
        assert_eq!(content["summary"]["qa_failed"], 1);
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "pm_audit");
        assert_eq!(content["graph_audit"]["flags_audited"], 2);
        assert_eq!(content["graph_audit"]["hard_violations"], 1);
        assert_eq!(content["graph_audit"]["advisory_flags"], 1);
        assert_eq!(
            content["graph_audit"]["runs"][0]["run"]["topology"]["graph"],
            "pm_audit"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("linear-pm-audit.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("pm graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"pm_audit\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn linear_health_requires_langgraph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("health-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__linear_health".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("Linear health LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn linear_health_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        write_linear_health_inputs(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("health-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_linear_health_graph_auditor(Arc::new(TestLinearHealthGraphAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__linear_health".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "health failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["issues_total"], 2);
        assert_eq!(content["summary"]["grade"], "healthy");
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "linear_health");
        assert_eq!(content["graph_audit"]["decision"], "healthy");
        assert_eq!(
            content["graph_audit"]["run"]["topology"]["graph"],
            "linear_health"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("linear-health.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("health graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"linear_health\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn dev_scorecard_requires_langgraph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("scorecard-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__dev_scorecard".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("developer scorecard LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn dev_scorecard_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        write_dev_scorecard_inputs(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("scorecard-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_dev_scorecard_graph_auditor(Arc::new(TestDevScorecardGraphAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__dev_scorecard".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "scorecard failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["devs_total"], 2);
        assert_eq!(content["summary"]["attribution_divergences"], 1);
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "dev_scorecard");
        assert_eq!(content["graph_audit"]["devs_audited"], 2);
        assert_eq!(content["graph_audit"]["attribution_divergences"], 1);
        assert_eq!(
            content["graph_audit"]["runs"][0]["run"]["topology"]["graph"],
            "dev_scorecard"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("dev-scorecard.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("scorecard graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"dev_scorecard\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn token_cost_requires_langgraph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("token-cost-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__token_cost".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("token cost LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn token_cost_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        write_token_cost_inputs(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("token-cost-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_token_cost_graph_auditor(Arc::new(TestTokenCostGraphAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__token_cost".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "token cost failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["tickets"], 2);
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "token_cost");
        assert_eq!(content["graph_audit"]["decision"], "cache-effective");
        assert_eq!(
            content["graph_audit"]["run"]["topology"]["graph"],
            "token_cost"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("token-cost.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("token cost graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"token_cost\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn tokens_scan_requires_langgraph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("tokens-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__tokens_scan".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("token usage LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn tokens_scan_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("tokens-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_token_usage_graph_auditor(Arc::new(TestTokenUsageGraphAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__tokens_scan".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "tokens scan failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["total_sessions"], 0);
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "token_usage");
        assert_eq!(content["graph_audit"]["decision"], "no-data");
        assert_eq!(
            content["graph_audit"]["run"]["topology"]["graph"],
            "token_usage"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("tokens-per-ticket.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("token usage graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"token_usage\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn eval_run_requires_langgraph_runner() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("eval-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__eval_run".into(),
                arguments: serde_json::json!({
                    "run_id": "mcp-eval",
                    "candidates_path": tmp.path().join("candidates.json"),
                }),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("eval LangGraph run port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn eval_run_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        let runs_dir = tmp.path().join("runs");
        let candidates_path = tmp.path().join("candidates.json");
        std::fs::write(&candidates_path, "{}").expect("candidate fixture");

        let state = Arc::new(RwLock::new(SessionState::new("eval-graph")));
        let engine = test_engine(state.clone());
        let handler =
            McpHandler::new(state, engine).with_eval_runner(Arc::new(TestEvalRunGraphRunner));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__eval_run".into(),
                arguments: serde_json::json!({
                    "run_id": "mcp-eval",
                    "candidates_path": candidates_path,
                    "runs_dir": runs_dir,
                    "case_ids": ["case-a"],
                }),
            })
            .await;

        assert!(result.success, "eval run failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["run"]["run_id"], "mcp-eval");
        assert_eq!(content["run"]["case_results"][0]["case_id"], "case-a");
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "eval");
        assert_eq!(content["graph_audit"]["decision"], "strong");
        assert_eq!(content["graph_audit"]["run"]["topology"]["graph"], "eval");

        let graph_jsonl = tmp.path().join("runs").join("mcp-eval.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("eval graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"eval\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ba_draft_requires_langgraph_runner() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("ba-draft-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__ba_draft".into(),
                arguments: serde_json::json!({
                    "brief": "scale the platform",
                    "audience": "exec",
                    "constraints": ["no PII"],
                }),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("BA draft LangGraph run port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ba_draft_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("ba-draft-graph")));
        let engine = test_engine(state.clone());
        let handler =
            McpHandler::new(state, engine).with_ba_draft_runner(Arc::new(TestBaDraftGraphRunner));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__ba_draft".into(),
                arguments: serde_json::json!({
                    "brief": "scale the platform",
                    "audience": "exec",
                    "constraints": ["no PII"],
                    "agent_id": "ba-orchestrator",
                }),
            })
            .await;

        assert!(result.success, "BA draft failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(
            content["recommendation"]["recommendation_id"],
            "mcp-ba-draft"
        );
        assert_eq!(content["recommendation"]["agent_id"], "ba-orchestrator");
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "ba_draft");
        assert_eq!(content["graph_audit"]["decision"], "high-risk-ready");
        assert_eq!(
            content["graph_audit"]["run"]["topology"]["graph"],
            "ba_draft"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("ba-draft")
            .join("mcp-ba-draft.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("BA draft graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"ba_draft\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn severity_scan_requires_langgraph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("severity-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine).with_llm(Arc::new(FixedSeverityLlm));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__severity_scan".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("severity LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn severity_scan_returns_langgraph_audit_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        write_linear_cache(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("severity-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_llm(Arc::new(FixedSeverityLlm))
            .with_severity_graph_auditor(Arc::new(TestSeverityGraphAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__severity_scan".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "severity scan failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["tickets_scanned"], 1);
        assert_eq!(content["summary"]["would_set"], 1);
        assert_eq!(content["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(content["graph_audit"]["graph"], "severity");
        assert_eq!(content["graph_audit"]["proposals_audited"], 1);
        assert_eq!(content["graph_audit"]["authorized_sets"], 1);
        assert_eq!(content["graph_audit"]["runs"][0]["identifier"], "FPCRM-777");
        assert_eq!(content["graph_audit"]["runs"][0]["decision"], "set");
        assert!(
            content["graph_audit"]["runs"][0]["authorization_checkpoint"]
                .as_str()
                .is_some_and(|checkpoint| checkpoint.contains("checkpoint-1"))
        );
        assert_eq!(
            content["graph_audit"]["runs"][0]["run"]["topology"]["graph"],
            "severity"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("severity.graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("graph audit jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"severity\""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn linear_code_audit_requires_reconciliation_graph_auditor() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("code-audit-no-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__linear_code_audit".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|err| err.contains("reconciliation LangGraph audit port")),
            "unexpected error: {:?}",
            result.error
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn linear_code_audit_returns_reconciliation_graph_evidence() {
        let _lock = HOME_ENV_LOCK.lock().expect("home env lock");
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _home = HomeGuard::set(tmp.path());
        write_code_audit_inputs(tmp.path());

        let state = Arc::new(RwLock::new(SessionState::new("code-audit-graph")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine)
            .with_code_reconciliation_auditor(Arc::new(TestCodeReconciliationAuditor));

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__linear_code_audit".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(result.success, "code audit failed: {:?}", result.error);
        let content = result.content;
        assert_eq!(content["workflow_authority"], "langgraph");
        assert_eq!(content["summary"]["completed_total"], 1);
        assert_eq!(content["summary"]["without_evidence"], 1);
        assert_eq!(
            content["reconciliation_audit"]["workflow_authority"],
            "langgraph"
        );
        assert_eq!(content["reconciliation_audit"]["graph"], "reconciliation");
        assert_eq!(content["reconciliation_audit"]["flags_audited"], 1);
        assert_eq!(content["reconciliation_audit"]["authorized_flags"], 1);
        assert_eq!(
            content["reconciliation_audit"]["runs"][0]["identifier"],
            "FPCRM-888"
        );
        assert_eq!(
            content["reconciliation_audit"]["runs"][0]["decision"],
            "flag"
        );
        assert_eq!(
            content["reconciliation_audit"]["runs"][0]["run"]["topology"]["graph"],
            "reconciliation"
        );

        let graph_jsonl = tmp
            .path()
            .join(".claude")
            .join("sentinel")
            .join("metrics")
            .join("linear-code-audit.reconciliation-graph-runs.jsonl");
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("reconciliation jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"reconciliation\""));
    }

    async fn handler_with_chain() -> McpHandler {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine.clone())
            .with_default_test_workflows()
            .with_mcp_proof_read_graph_auditor(Arc::new(TestMcpProofReadGraphAuditor))
            .with_archive(empty_archive_backing());

        // Seed chain: 2 step proofs in phase "claim".
        engine
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"ticket": "FPCRM-1"}),
                Some("firefly-pro".into()),
                Utc::now(),
            )
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        engine
            .submit_step_evidence(
                "linear",
                "claim",
                "2",
                "create branch",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::json!({"branch": "fpcrm-1-fix"}),
                Some("firefly-pro".into()),
                Utc::now(),
            )
            .await
            .unwrap();

        handler
    }

    fn assert_mcp_proof_read_graph_audit(payload: &serde_json::Value, surface: &str) {
        assert_eq!(
            payload.pointer("/graph_audit/workflow_authority"),
            Some(&serde_json::json!("langgraph"))
        );
        assert_eq!(
            payload.pointer("/graph_audit/graph"),
            Some(&serde_json::json!("mcp_proof_read"))
        );
        assert_eq!(
            payload.pointer("/graph_audit/surface"),
            Some(&serde_json::json!(surface))
        );
        assert!(payload
            .pointer("/graph_audit/authorization_checkpoint")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|checkpoint| checkpoint.contains('#')));
    }

    async fn record_independent_verdict(
        state: &Arc<RwLock<SessionState>>,
        phase_id: &str,
        step_id: &str,
        sufficient: bool,
        confidence: f64,
    ) {
        state
            .write()
            .await
            .record_independent_verdict("linear", phase_id, step_id, sufficient, confidence);
    }

    async fn record_handler_independent_verdict(
        handler: &McpHandler,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        sufficient: bool,
        confidence: f64,
    ) {
        handler
            .state
            .write()
            .await
            .record_independent_verdict(skill, phase_id, step_id, sufficient, confidence);
    }

    fn step_started_at() -> String {
        Utc::now().to_rfc3339()
    }

    fn empty_evidence_json() -> serde_json::Value {
        serde_json::json!({
            "tool_calls": [],
            "tool_results": [],
            "files_changed": [],
            "phase_file_read": false,
        })
    }

    #[tokio::test]
    async fn unknown_step_tool_name_errors_with_clear_message() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__nonexistent".into(),
                arguments: serde_json::json!({}),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown tool"));
    }

    #[tokio::test]
    async fn get_proof_chain_returns_mcp_read_graph_audit() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_proof_chain".into(),
                arguments: serde_json::json!({"skill": "linear"}),
            })
            .await;
        assert!(result.success, "error: {:?}", result.error);
        let payload = result.content;
        assert_eq!(
            payload.get("workflow_authority").and_then(|v| v.as_str()),
            Some("langgraph")
        );
        assert_eq!(
            payload
                .get("graph_workflow")
                .and_then(|v| v.get("skill"))
                .and_then(|v| v.as_str()),
            Some("linear")
        );
        assert!(payload.get("entries").and_then(|v| v.as_array()).is_some());
        assert_mcp_proof_read_graph_audit(&payload, "proof_chain");
    }

    #[tokio::test]
    async fn get_step_proof_returns_matching_step() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_proof".into(),
                arguments: serde_json::json!({"skill": "linear", "step_id": "1"}),
            })
            .await;
        assert!(result.success, "error: {:?}", result.error);
        let proof = result.content;
        assert_eq!(proof.get("step_id").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(proof.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(
            proof.get("workflow_authority").and_then(|v| v.as_str()),
            Some("langgraph")
        );
        assert_eq!(
            proof
                .get("graph_workflow")
                .and_then(|v| v.get("skill"))
                .and_then(|v| v.as_str()),
            Some("linear")
        );
        assert_mcp_proof_read_graph_audit(&proof, "step_proof");
        assert_eq!(
            proof.get("phase_id").and_then(|v| v.as_str()),
            Some("claim")
        );
        assert!(proof.get("combined_hash").is_some());
    }

    #[tokio::test]
    async fn get_step_proof_404s_for_missing_step() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_proof".into(),
                arguments: serde_json::json!({"skill": "linear", "step_id": "99"}),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("step_id '99'"));
    }

    #[tokio::test]
    async fn get_step_proof_filters_by_phase_when_supplied() {
        let handler = handler_with_chain().await;
        // step_id "1" exists in phase "claim". Asking for it under
        // a phase that doesn't contain it must 404.
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_proof".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "step_id": "1",
                    "phase_id": "review", // wrong phase
                }),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("phase 'review'"));
    }

    #[tokio::test]
    async fn get_step_proof_requires_skill_and_step_id() {
        let handler = handler_with_chain().await;
        for bad_args in [
            serde_json::json!({}),                  // missing both
            serde_json::json!({"skill": "linear"}), // missing step_id
            serde_json::json!({"step_id": "1"}),    // missing skill
        ] {
            let result = handler
                .handle(McpToolCall {
                    name: "sentinel__get_step_proof".into(),
                    arguments: bad_args,
                })
                .await;
            assert!(!result.success);
        }
    }

    #[tokio::test]
    async fn get_step_chain_returns_all_steps_in_order() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_chain".into(),
                arguments: serde_json::json!({"skill": "linear"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(
            payload.get("skill").and_then(|v| v.as_str()),
            Some("linear")
        );
        assert_eq!(
            payload.get("workflow_authority").and_then(|v| v.as_str()),
            Some("langgraph")
        );
        assert_eq!(
            payload
                .get("graph_workflow")
                .and_then(|v| v.get("skill"))
                .and_then(|v| v.as_str()),
            Some("linear")
        );
        assert_mcp_proof_read_graph_audit(&payload, "step_chain");
        assert_eq!(payload.get("step_count").and_then(|v| v.as_u64()), Some(2));
        let steps = payload.get("steps").and_then(|v| v.as_array()).unwrap();
        assert_eq!(steps.len(), 2);
        // Order check — step "1" before step "2" in the array.
        assert_eq!(steps[0].get("step_id").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(steps[1].get("step_id").and_then(|v| v.as_str()), Some("2"));
        // head_hash matches the last step's combined_hash.
        let last_combined = steps[1]
            .get("combined_hash")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(
            payload.get("head_hash").and_then(|v| v.as_str()),
            Some(last_combined)
        );
    }

    #[tokio::test]
    async fn get_step_chain_404s_for_unknown_skill() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_chain".into(),
                arguments: serde_json::json!({"skill": "nonexistent"}),
            })
            .await;
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("nonexistent"));
    }

    #[tokio::test]
    async fn get_active_step_reports_last_step_and_counts() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_active_step".into(),
                arguments: serde_json::json!({"skill": "linear"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(
            payload.get("skill").and_then(|v| v.as_str()),
            Some("linear")
        );
        assert_eq!(
            payload.get("workflow_authority").and_then(|v| v.as_str()),
            Some("langgraph")
        );
        assert_eq!(
            payload
                .get("graph_workflow")
                .and_then(|v| v.get("skill"))
                .and_then(|v| v.as_str()),
            Some("linear")
        );
        assert_mcp_proof_read_graph_audit(&payload, "active_step");
        assert_eq!(payload.get("step_count").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(payload.get("phase_count").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(
            payload.get("chain_length").and_then(|v| v.as_u64()),
            Some(2)
        );
        let last = payload.get("last_step").unwrap();
        assert_eq!(last.get("step_id").and_then(|v| v.as_str()), Some("2"));
        assert_eq!(last.get("phase_id").and_then(|v| v.as_str()), Some("claim"));
        assert!(last.get("combined_hash").is_some());
    }

    // ─────────────────────────────────────────────────────────────────
    // M4.2: submit_step_complete tests
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn submit_step_complete_rejects_missing_independent_verdict() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine).with_default_test_workflows();

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.93,
                        "reasoning": "caller says evidence present",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.contains("missing independent step_judge verdict"),
            "error must reject missing independent verdict: {err}"
        );
        assert!(
            err.contains("not accepted as a substitute"),
            "error must explicitly reject self-certification: {err}"
        );
    }

    #[tokio::test]
    async fn submit_step_complete_requires_explicit_workflow_catalog() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        record_independent_verdict(&state, "claim", "1", true, 0.93).await;
        let handler = McpHandler::new(state.clone(), engine);

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.93,
                        "reasoning": "caller says evidence present",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.contains("requires configured LangGraph workflow context"),
            "error must identify missing workflow catalog: {err}"
        );
        let state = state.read().await;
        assert!(
            state.proof_chains_is_empty(),
            "missing workflow catalog must not seal a StepProof"
        );
        assert!(
            !state.has_any_graph_workflow(),
            "missing workflow catalog must not synthesize workflow state"
        );
    }

    #[tokio::test]
    async fn submit_step_complete_seals_step_with_required_args() {
        // Smallest legal call: skill + phase_id + step_id + step_description +
        // verdict + evidence + started_at. Optional non-proof payload fields
        // can be omitted.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        record_independent_verdict(&state, "claim", "1", true, 0.93).await;
        let handler = McpHandler::new(state, engine).with_default_test_workflows();

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.93,
                        "reasoning": "evidence present",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;

        assert!(result.success, "error: {:?}", result.error);
        let proof = result.content;
        assert_eq!(proof.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(proof.get("step_id").and_then(|v| v.as_str()), Some("1"));
        assert_eq!(
            proof.get("phase_id").and_then(|v| v.as_str()),
            Some("claim")
        );
        assert_eq!(
            proof.get("workflow_authority").and_then(|v| v.as_str()),
            Some("langgraph")
        );
        assert_eq!(
            proof.get("graph_state"),
            proof.pointer("/phase_graph/graph_state")
        );
        assert_eq!(
            proof.get("latest_checkpoint"),
            proof.pointer("/phase_graph/latest_checkpoint")
        );
        assert!(proof.get("combined_hash").is_some());
        // The judge tier is taken from the configured workflow phase, not from
        // caller input.
        assert_eq!(
            proof.get("judge_model").and_then(|v| v.as_str()),
            Some("anthropic/claude-sonnet-4.6"),
        );
    }

    #[tokio::test]
    async fn submit_step_complete_propagates_artifact_and_account_context() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        record_independent_verdict(&state, "claim", "1", true, 0.95).await;
        let handler = McpHandler::new(state, engine).with_default_test_workflows();

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "open PR",
                    "verdict": {"sufficient": true, "confidence": 0.95, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "artifact": {"pr_url": "https://github.com/foo/bar/pull/9", "pr_number": 9},
                    "account_context": "firefly-pro",
                }),
            })
            .await;

        assert!(result.success);
        let proof = result.content;
        assert_eq!(
            proof.get("account_context").and_then(|v| v.as_str()),
            Some("firefly-pro"),
        );
        assert_eq!(
            proof
                .get("artifact")
                .and_then(|v| v.get("pr_url"))
                .and_then(|v| v.as_str()),
            Some("https://github.com/foo/bar/pull/9"),
        );
        assert_eq!(
            proof.get("judge_model").and_then(|v| v.as_str()),
            Some("anthropic/claude-sonnet-4.6"),
        );
    }

    #[tokio::test]
    async fn submit_step_complete_rejects_insufficient_verdict() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();
        record_independent_verdict(&state, "claim", "1", false, 0.7).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": false,
                        "confidence": 0.7,
                        "reasoning": "missing FPCRM ref in PR body",
                        "requested_evidence": ["Ref FPCRM-XXX in PR body"],
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.to_lowercase().contains("insufficient"),
            "error mentions insufficient: {err}"
        );
        // No chain mutation on failure.
        let s = state.read().await;
        assert!(!s.has_proof_chain("linear"));
    }

    #[tokio::test]
    async fn submit_step_complete_validates_required_args() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        record_independent_verdict(&state, "claim", "1", true, 0.9).await;
        let handler = McpHandler::new(state, engine).with_default_test_workflows();

        // Each entry below is missing exactly one required field.
        let cases = [
            (serde_json::json!({}), "skill"),
            (serde_json::json!({"skill": "linear"}), "phase_id"),
            (
                serde_json::json!({"skill": "linear", "phase_id": "claim"}),
                "step_id",
            ),
            (
                serde_json::json!({
                    "skill": "linear", "phase_id": "claim", "step_id": "1"
                }),
                "step_description",
            ),
            (
                serde_json::json!({
                    "skill": "linear", "phase_id": "claim", "step_id": "1",
                    "step_description": "fetch",
                }),
                "verdict",
            ),
            (
                serde_json::json!({
                    "skill": "linear", "phase_id": "claim", "step_id": "1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                }),
                "evidence",
            ),
            (
                serde_json::json!({
                    "skill": "linear", "phase_id": "claim", "step_id": "1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "evidence": empty_evidence_json(),
                }),
                "started_at",
            ),
        ];

        for (args, missing) in cases {
            let result = handler
                .handle(McpToolCall {
                    name: "sentinel__submit_step_complete".into(),
                    arguments: args,
                })
                .await;
            assert!(!result.success, "expected failure when missing {missing}");
            assert!(
                result.error.as_deref().unwrap().contains(missing),
                "error must name the missing arg '{missing}', got: {:?}",
                result.error,
            );
        }
    }

    #[tokio::test]
    async fn submit_step_complete_rejects_caller_supplied_judge_model() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        record_independent_verdict(&state, "claim", "1", true, 0.9).await;
        let handler = McpHandler::new(state, engine).with_default_test_workflows();

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "judge_model": "opus",
                }),
            })
            .await;

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap()
            .contains("workflow-configured authority"));
    }

    #[tokio::test]
    async fn submit_step_complete_chains_to_existing_proof() {
        // Two sequential submits via the MCP tool — second's previous_hash
        // must equal the first's combined_hash.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        record_independent_verdict(&state, "claim", "1", true, 0.95).await;
        let r1 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {"sufficient": true, "confidence": 0.95, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(r1.success);
        let combined_1 = r1
            .content
            .get("combined_hash")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // Brief pause so step 2's started_at > step 1's completed_at.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        record_independent_verdict(&state, "claim", "2", true, 0.95).await;
        let r2 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "2",
                    "step_description": "create branch",
                    "verdict": {"sufficient": true, "confidence": 0.95, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(r2.success);
        let prev_2 = r2
            .content
            .get("previous_hash")
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(
            prev_2, combined_1,
            "step 2 previous_hash must equal step 1 combined_hash via head_hash() resolution",
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // M4.3: query_proof_corpus tests
    // ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn query_proof_corpus_errors_without_archive_backing() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state, engine).with_default_test_workflows();

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({}),
            })
            .await;

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("proof archive backing is not configured"));
    }

    #[tokio::test]
    async fn query_proof_corpus_returns_summaries_for_live_chains() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(
            payload.get("scope").and_then(|v| v.as_str()),
            Some("live-session")
        );
        assert_eq!(
            payload.get("total_matched").and_then(|v| v.as_u64()),
            Some(1)
        );
        let chains = payload.get("chains").and_then(|v| v.as_array()).unwrap();
        assert_eq!(chains.len(), 1);
        let c0 = &chains[0];
        assert_eq!(c0.get("skill").and_then(|v| v.as_str()), Some("linear"));
        assert_eq!(c0.get("step_count").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(
            c0.get("all_sufficient").and_then(|v| v.as_bool()),
            Some(true)
        );
        // step_sequence is the pattern signal — exact ordered coordinates.
        let seq = c0.get("step_sequence").and_then(|v| v.as_array()).unwrap();
        let labels: Vec<&str> = seq.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(labels, vec!["claim.1", "claim.2"]);
    }

    #[tokio::test]
    async fn query_proof_corpus_skill_filter_excludes_non_matches() {
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({"skill_filter": "deploy"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(
            payload.get("total_matched").and_then(|v| v.as_u64()),
            Some(0)
        );
        assert!(payload
            .get("chains")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn query_proof_corpus_min_steps_filter_works() {
        let handler = handler_with_chain().await;
        // Chain has 2 step entries; min_steps=3 must exclude.
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({"min_steps": 3}),
            })
            .await;
        assert!(result.success);
        assert_eq!(
            result.content.get("total_matched").and_then(|v| v.as_u64()),
            Some(0),
        );

        // min_steps=2 must include.
        let result2 = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({"min_steps": 2}),
            })
            .await;
        assert!(result2.success);
        assert_eq!(
            result2
                .content
                .get("total_matched")
                .and_then(|v| v.as_u64()),
            Some(1),
        );
    }

    #[tokio::test]
    async fn query_proof_corpus_max_results_caps_returned_chains() {
        // Build a state with 3 chains; query with max_results=2 should
        // return 2 chains but report total_matched=3.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine.clone())
            .with_default_test_workflows()
            .with_archive(empty_archive_backing());

        for skill in ["linear", "git", "deploy"] {
            engine
                .submit_step_evidence(
                    skill,
                    "claim",
                    "1",
                    "fetch",
                    Evidence::default(),
                    JudgeVerdict::pass(0.95, "ok"),
                    JudgeModel::Sonnet,
                    serde_json::Value::Null,
                    None,
                    Utc::now() - chrono::Duration::milliseconds(10),
                )
                .await
                .unwrap();
        }

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({"max_results": 2}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(
            payload.get("total_matched").and_then(|v| v.as_u64()),
            Some(3)
        );
        let chains = payload.get("chains").and_then(|v| v.as_array()).unwrap();
        assert_eq!(chains.len(), 2, "max_results caps returned chains");
    }

    #[tokio::test]
    async fn query_proof_corpus_successful_only_filters_failed_chains() {
        // Hard to forge a "failed but sealed" chain — the engine refuses
        // to seal insufficient verdicts. So this test verifies the
        // *positive* case: a fully-sufficient chain is included by
        // default. A separate test would seed a chain with a manually
        // crafted failed StepProof, but that requires bypassing the
        // engine and belongs in a lower-level fixture.
        let handler = handler_with_chain().await;
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__query_proof_corpus".into(),
                arguments: serde_json::json!({"successful_only": true}),
            })
            .await;
        assert!(result.success);
        let chains = result
            .content
            .get("chains")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(chains.len(), 1);
        assert_eq!(
            chains[0].get("all_sufficient").and_then(|v| v.as_bool()),
            Some(true),
        );
    }

    #[tokio::test]
    async fn get_active_step_returns_null_last_step_when_chain_is_phase_only() {
        // Empty entries vec, no step proofs — last_step should be null.
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_mcp_proof_read_graph_auditor(Arc::new(TestMcpProofReadGraphAuditor));

        // Insert an empty chain manually (no proofs, no entries).
        {
            let mut s = state.write().await;
            s.restore_proof_chain(
                "phaseonly".to_string(),
                sentinel_domain::proof::ProofChain::new("phaseonly", "test-session"),
            );
            s.set_graph_projected_workflow(
                "phaseonly".to_string(),
                WorkflowState::new("phaseonly", "test-session"),
            );
        }

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_active_step".into(),
                arguments: serde_json::json!({"skill": "phaseonly"}),
            })
            .await;
        assert!(result.success);
        let payload = result.content;
        assert_eq!(
            payload.get("workflow_authority").and_then(|v| v.as_str()),
            Some("langgraph")
        );
        assert_mcp_proof_read_graph_audit(&payload, "active_step");
        assert!(payload.get("last_step").unwrap().is_null());
        assert_eq!(payload.get("step_count").and_then(|v| v.as_u64()), Some(0));
    }

    #[tokio::test]
    async fn proof_reads_require_mcp_read_graph_auditor() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine.clone()).with_default_test_workflows();

        engine
            .submit_step_evidence(
                "linear",
                "claim",
                "1",
                "fetch ticket",
                Evidence::default(),
                JudgeVerdict::pass(0.95, "ok"),
                JudgeModel::Sonnet,
                serde_json::Value::Null,
                None,
                Utc::now(),
            )
            .await
            .unwrap();

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__get_step_chain".into(),
                arguments: serde_json::json!({"skill": "linear"}),
            })
            .await;

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|err| err.contains("MCP proof read LangGraph audit port")));
    }

    #[tokio::test]
    async fn proof_reads_require_langgraph_workflow_projection() {
        let state = Arc::new(RwLock::new(SessionState::new("test-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        {
            let mut s = state.write().await;
            s.restore_proof_chain(
                "linear".to_string(),
                sentinel_domain::proof::ProofChain::new("linear", "test-session"),
            );
        }

        for (tool, arguments) in [
            (
                "sentinel__get_proof_chain",
                serde_json::json!({"skill": "linear"}),
            ),
            (
                "sentinel__get_step_proof",
                serde_json::json!({"skill": "linear", "step_id": "1"}),
            ),
            (
                "sentinel__get_step_chain",
                serde_json::json!({"skill": "linear"}),
            ),
            (
                "sentinel__get_active_step",
                serde_json::json!({"skill": "linear"}),
            ),
        ] {
            let result = handler
                .handle(McpToolCall {
                    name: tool.into(),
                    arguments,
                })
                .await;
            assert!(!result.success, "{tool} must fail without graph projection");
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|err| err.contains("LangGraph-projected workflow state")),
                "unexpected {tool} error: {:?}",
                result.error
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // M5.1: End-to-end Backlog → Code Review pipeline (sentinel #42)
    // ─────────────────────────────────────────────────────────────────
    //
    // Drives the same submit_step_complete path Claude takes in real
    // linear-skill execution, end-to-end through the phases that bring
    // a ticket from Backlog → Code Review (claim → fetch → intelligence
    // → worktree → review). Asserts each step seals into the chain,
    // hashes link Merkle-style, and an `insufficient` judge verdict at
    // any step halts the chain (gate held). Does NOT drive real Linear
    // or real GitHub — that's the manual recipe in
    // `docs/m5-linear-e2e-runbook.md`.

    fn ok_verdict(reasoning: &str) -> serde_json::Value {
        serde_json::json!({
            "sufficient": true,
            "confidence": 0.92,
            "reasoning": reasoning,
        })
    }

    async fn submit_linear_step(
        handler: &McpHandler,
        phase_id: &str,
        step_id: &str,
        description: &str,
        artifact: serde_json::Value,
        reasoning: &str,
    ) -> McpToolResult {
        record_handler_independent_verdict(handler, "linear", phase_id, step_id, true, 0.92).await;
        handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": phase_id,
                    "step_id": step_id,
                    "step_description": description,
                    "verdict": ok_verdict(reasoning),
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "artifact": artifact,
                    "account_context": "firefly-pro",
                }),
            })
            .await
    }

    #[tokio::test]
    async fn m5_1_backlog_to_code_review_pipeline_seals_chain_in_order() {
        let state = Arc::new(RwLock::new(SessionState::new("m5-1-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        let pipeline: Vec<(&str, &str, &str, serde_json::Value)> = vec![
            (
                "claim",
                "0.1",
                "Set FPCRM-100 to In Progress and assign to viewer",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "previous_state": "Backlog",
                    "new_state": "In Progress",
                }),
            ),
            (
                "fetch",
                "1.1",
                "Fetch issue with relations + comments + attachments",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "comment_count": 3,
                    "attachment_count": 1,
                    "labels": ["bug", "area:auth"],
                }),
            ),
            (
                "intelligence",
                "1.5.2",
                "Size as Small (2 deliverables) and transform missing fields",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "complexity": "small",
                    "deliverables": 2,
                    "fields_fixed": ["estimate", "type_label"],
                }),
            ),
            (
                "worktree",
                "2.1",
                "Create git worktree fpcrm-100-fix-auth and run baseline tests",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "branch": "fix/fpcrm-100-auth",
                    "worktree_path": "../fpcrm-100-fix-auth",
                    "baseline_tests": {"passed": 412, "failed": 0},
                }),
            ),
            (
                "worktree",
                "2.5",
                "Implement fix and verify tests still green",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "branch": "fix/fpcrm-100-auth",
                    "files_changed": 3,
                    "post_impl_tests": {"passed": 414, "failed": 0},
                }),
            ),
            (
                "review",
                "3.L0",
                "Test validation pass — zero regressions vs baseline",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "delta": {"new_pass": 2, "regressions": 0},
                }),
            ),
            (
                "review",
                "3.L3",
                "Push branch, open PR, transition Linear to Code Review",
                serde_json::json!({
                    "issue_id": "FPCRM-100",
                    "pr_url": "https://github.com/firefly-pro/firefly-pro-crm/pull/4242",
                    "pr_number": 4242,
                    "linear_state": "Code Review",
                }),
            ),
        ];

        let mut sealed_hashes: Vec<String> = Vec::new();
        for (phase_id, step_id, description, artifact) in &pipeline {
            let result = submit_linear_step(
                &handler,
                phase_id,
                step_id,
                description,
                artifact.clone(),
                "claude provided evidence; judge satisfied",
            )
            .await;
            assert!(
                result.success,
                "step {phase_id}/{step_id} sealed: {:?}",
                result.error
            );
            let proof = result.content;
            assert_eq!(
                proof.get("phase_id").and_then(|v| v.as_str()),
                Some(*phase_id)
            );
            assert_eq!(
                proof.get("step_id").and_then(|v| v.as_str()),
                Some(*step_id)
            );
            let hash = proof
                .get("combined_hash")
                .and_then(|v| v.as_str())
                .expect("sealed step has combined_hash")
                .to_string();
            assert!(!hash.is_empty(), "combined_hash must not be empty");
            sealed_hashes.push(hash);
        }

        let s = state.read().await;
        let chain = s
            .proof_chain("linear")
            .expect("linear chain exists after pipeline run");
        assert_eq!(
            chain.entries.len(),
            pipeline.len(),
            "chain should have one entry per submission"
        );

        let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();

        let phase_sequence: Vec<&str> = step_entries.iter().map(|s| s.phase_id.as_str()).collect();
        let expected_sequence: Vec<&str> = pipeline.iter().map(|(p, _, _, _)| *p).collect();
        assert_eq!(phase_sequence, expected_sequence);

        let step_sequence: Vec<&str> = step_entries.iter().map(|s| s.step_id.as_str()).collect();
        let expected_step_sequence: Vec<&str> = pipeline.iter().map(|(_, s, _, _)| *s).collect();
        assert_eq!(step_sequence, expected_step_sequence);

        let unique_hashes: std::collections::HashSet<_> = sealed_hashes.iter().collect();
        assert_eq!(
            unique_hashes.len(),
            sealed_hashes.len(),
            "every sealed step must have a distinct combined_hash"
        );

        let final_step = step_entries.last().expect("at least one step in chain");
        assert_eq!(final_step.phase_id, "review");
        assert_eq!(final_step.step_id, "3.L3");
    }

    #[tokio::test]
    async fn m5_1_insufficient_verdict_halts_pipeline_midflight() {
        let state = Arc::new(RwLock::new(SessionState::new("m5-1-halt-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        let r1 = submit_linear_step(
            &handler,
            "claim",
            "0.1",
            "claim",
            serde_json::json!({"issue_id": "FPCRM-101"}),
            "ok",
        )
        .await;
        assert!(r1.success);

        let r2 = submit_linear_step(
            &handler,
            "fetch",
            "1.1",
            "fetch issue",
            serde_json::json!({"issue_id": "FPCRM-101"}),
            "ok",
        )
        .await;
        assert!(r2.success);

        let chain_len_before_halt = {
            let s = state.read().await;
            s.proof_chain("linear").unwrap().entries.len()
        };
        assert_eq!(chain_len_before_halt, 2);

        record_handler_independent_verdict(&handler, "linear", "review", "3.L3", false, 0.7).await;
        let halt = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "review",
                    "step_id": "3.L3",
                    "step_description": "open PR without FPCRM ref",
                    "verdict": {
                        "sufficient": false,
                        "confidence": 0.7,
                        "reasoning": "PR body missing Ref FPCRM-101",
                        "requested_evidence": ["Add Ref FPCRM-101 to PR body"],
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "artifact": {"pr_url": "https://github.com/foo/bar/pull/1"},
                    "account_context": "firefly-pro",
                }),
            })
            .await;
        assert!(!halt.success, "insufficient verdict must not seal");

        let chain_len_after_halt = {
            let s = state.read().await;
            s.proof_chain("linear").unwrap().entries.len()
        };
        assert_eq!(
            chain_len_after_halt, chain_len_before_halt,
            "insufficient verdict must not extend the chain"
        );

        let r3 = submit_linear_step(
            &handler,
            "review",
            "3.L3",
            "open PR with FPCRM ref",
            serde_json::json!({
                "issue_id": "FPCRM-101",
                "pr_url": "https://github.com/foo/bar/pull/1",
                "linear_state": "Code Review",
            }),
            "evidence corrected",
        )
        .await;
        assert!(r3.success);

        let chain_len_after_retry = {
            let s = state.read().await;
            s.proof_chain("linear").unwrap().entries.len()
        };
        assert_eq!(chain_len_after_retry, chain_len_before_halt + 1);
    }

    // ─────────────────────────────────────────────────────────────────
    // M5.2: End-to-end Code Review → QA Testing → Completed (sentinel #43)
    // ─────────────────────────────────────────────────────────────────
    //
    // Picks up where M5.1 left off (PR open, Linear at Code Review) and
    // drives the back half of the pipeline:
    //
    //   review (3.L4) — CI green + CodeRabbit triaged
    //   review (3.L5) — Merge to main
    //   qa-handoff (3.5.0)   — Deploy to staging
    //   qa-handoff (3.5.1)   — Browserbase smoke test on staging
    //   qa-handoff (3.5.2)   — Loom upload of smoke screenshots
    //   qa-handoff (3.5.3-4) — Transition Linear to QA Testing + assign
    //   qa-handoff (3.5.5)   — Implementation comment with evidence
    //   cleanup (4.1)        — Remove worktree
    //
    // ...then simulates the QA tester pass that transitions Linear from
    // QA Testing → Completed. M5.1 covered M2.3.A-F (claim through review);
    // M5.2 covers M2.3.G-H (qa-handoff and cleanup) + the QA-pass tail.
    //
    // Also asserts proof chain integrity across the entire 14-step
    // journey from Backlog → Completed when composed with the M5.1
    // pipeline. This is the full-lifecycle smoke test the M5 epic
    // (#41) needs to consider "the chain works end-to-end."

    #[tokio::test]
    async fn m5_2_code_review_to_completed_seals_qa_handoff_and_cleanup() {
        let state = Arc::new(RwLock::new(SessionState::new("m5-2-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        // Pre-seed: assume M5.1 already sealed claim → review/3.L3. We
        // only need ONE prior step in the chain to test continuity; the
        // proof engine's prev_hash chaining doesn't care how many
        // earlier entries exist, only that they exist in order.
        let _seed = submit_linear_step(
            &handler,
            "review",
            "3.L3",
            "Open PR + transition to Code Review",
            serde_json::json!({
                "issue_id": "FPCRM-200",
                "pr_url": "https://github.com/firefly-pro/firefly-pro-crm/pull/4300",
                "linear_state": "Code Review",
            }),
            "PR open, Linear at Code Review",
        )
        .await;
        assert!(_seed.success);

        // The back-half pipeline. Each tuple is
        // (phase_id, step_id, description, artifact).
        let back_half: Vec<(&str, &str, &str, serde_json::Value)> = vec![
            (
                "review",
                "3.L4",
                "CI green + CodeRabbit comments triaged + approvals",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "pr_number": 4300,
                    "ci_runs": [{"name": "test", "conclusion": "success"}],
                    "coderabbit_comments": {"actionable": 2, "addressed": 2, "nitpick": 5},
                    "approvals": 1,
                }),
            ),
            (
                "review",
                "3.L5",
                "Merge PR to main and confirm post-merge state",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "merge_commit": "abc123def",
                    "merge_strategy": "squash",
                }),
            ),
            (
                "qa-handoff",
                "3.5.0",
                "Deploy main to staging and wait for healthy",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "staging_url": "https://staging.firefly-pro.example/",
                    "deploy_id": "deploy-9921",
                    "healthy": true,
                }),
            ),
            (
                "qa-handoff",
                "3.5.1",
                "Browserbase smoke test on staging — feature reachable, no console errors",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "staging_url": "https://staging.firefly-pro.example/",
                    "screenshots": 4,
                    "console_errors": 0,
                    "browserbase_session": "bb-session-7733",
                }),
            ),
            (
                "qa-handoff",
                "3.5.2",
                "Compile screenshots to Loom video for QA reference",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "loom_url": "https://loom.com/share/abc123",
                    "duration_seconds": 18,
                }),
            ),
            (
                "qa-handoff",
                "3.5.3",
                "Transition Linear from Code Review to QA Testing",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "previous_state": "Code Review",
                    "new_state": "QA Testing",
                }),
            ),
            (
                "qa-handoff",
                "3.5.4",
                "Reassign Linear ticket to QA tester",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "qa_assignee_email": "qa@fireflypro.com",
                }),
            ),
            (
                "qa-handoff",
                "3.5.5",
                "Post implementation comment with Loom link + test results",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "comment_id": "comment-554",
                    "includes_loom": true,
                    "includes_test_summary": true,
                }),
            ),
            (
                "cleanup",
                "4.1",
                "Remove git worktree after merge",
                serde_json::json!({
                    "issue_id": "FPCRM-200",
                    "worktree_path": "../fpcrm-200-fix",
                    "branch_deleted": "fix/fpcrm-200",
                }),
            ),
        ];

        let mut last_hash: Option<String> = None;
        for (phase_id, step_id, description, artifact) in &back_half {
            let result = submit_linear_step(
                &handler,
                phase_id,
                step_id,
                description,
                artifact.clone(),
                "evidence sufficient for handoff step",
            )
            .await;
            assert!(
                result.success,
                "step {phase_id}/{step_id} should seal: {:?}",
                result.error
            );
            let hash = result
                .content
                .get("combined_hash")
                .and_then(|v| v.as_str())
                .map(String::from)
                .expect("combined_hash present");

            // Each step's hash must differ from the previous step's — the
            // engine has to fold prev_hash into the next hash. If two
            // consecutive sealed steps share a hash, the chain has been
            // forked or the engine is broken.
            if let Some(prev) = &last_hash {
                assert_ne!(
                    &hash, prev,
                    "consecutive step hashes must differ; engine forgot to fold prev_hash"
                );
            }
            last_hash = Some(hash);
        }

        // Simulate the QA-tester pass — they don't go through sentinel,
        // they update Linear state directly. We model it as a final step
        // sealed by the QA bot, marking the end of the lifecycle.
        let qa_pass = submit_linear_step(
            &handler,
            "qa-handoff",
            "3.5.6",
            "QA tester pass — Linear transitions QA Testing → Completed",
            serde_json::json!({
                "issue_id": "FPCRM-200",
                "previous_state": "QA Testing",
                "new_state": "Completed",
                "qa_tester": "qa@fireflypro.com",
                "qa_pass_timestamp": "2026-05-14T10:00:00Z",
            }),
            "QA verified feature works on staging",
        )
        .await;
        assert!(qa_pass.success);

        let s = state.read().await;
        let chain = s.proof_chain("linear").expect("linear chain");

        // 1 seed + 9 back-half + 1 QA pass = 11 entries total.
        assert_eq!(
            chain.entries.len(),
            11,
            "M5.2 back-half + QA-pass yields 11 sealed steps"
        );

        // The final step lands at Completed, the lifecycle's true endpoint.
        let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();
        let final_step = step_entries.last().unwrap();
        assert_eq!(final_step.phase_id, "qa-handoff");
        assert_eq!(final_step.step_id, "3.5.6");
        assert_eq!(
            final_step
                .artifact
                .get("new_state")
                .and_then(|v| v.as_str()),
            Some("Completed")
        );

        // Audit-trail check: every step's artifact carries the same
        // issue_id, so a future query on the chain can reconstruct the
        // full life of FPCRM-200 from this skill's proof chain alone.
        for step in &step_entries {
            assert_eq!(
                step.artifact.get("issue_id").and_then(|v| v.as_str()),
                Some("FPCRM-200"),
                "every step must carry the issue_id forward"
            );
        }
    }

    #[tokio::test]
    async fn m5_2_qa_fail_keeps_ticket_at_qa_testing() {
        // The flip side of M5.2: when QA fails, the ticket goes to
        // "QA Failed" (not Completed). Sentinel doesn't care about the
        // Linear state name — what matters is that the proof chain
        // honestly records the QA verdict and doesn't pretend success.
        let state = Arc::new(RwLock::new(SessionState::new("m5-2-fail-session")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        // Drive to QA-Testing state.
        for (phase_id, step_id, desc) in [
            ("review", "3.L5", "merge"),
            ("qa-handoff", "3.5.0", "deploy"),
            ("qa-handoff", "3.5.3", "transition to QA Testing"),
        ] {
            let r = submit_linear_step(
                &handler,
                phase_id,
                step_id,
                desc,
                serde_json::json!({"issue_id": "FPCRM-201"}),
                "ok",
            )
            .await;
            assert!(r.success);
        }

        // QA fail — sealed honestly with the new_state = QA Failed.
        let qa_fail = submit_linear_step(
            &handler,
            "qa-handoff",
            "3.5.6",
            "QA tester rejects — bug found in staging Browserbase test",
            serde_json::json!({
                "issue_id": "FPCRM-201",
                "previous_state": "QA Testing",
                "new_state": "QA Failed",
                "rejection_reason": "Login button broken on Safari iOS",
            }),
            "QA rejection recorded",
        )
        .await;
        assert!(qa_fail.success);

        let s = state.read().await;
        let chain = s.proof_chain("linear").unwrap();
        let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();
        let final_step = step_entries.last().unwrap();

        // Crucial: the chain reports QA Failed, NOT Completed. The
        // honest record is the whole point of proof chains — they
        // can't lie about what happened.
        assert_eq!(
            final_step
                .artifact
                .get("new_state")
                .and_then(|v| v.as_str()),
            Some("QA Failed")
        );
        assert!(
            final_step.artifact.get("rejection_reason").is_some(),
            "QA failure must carry a rejection_reason for the audit trail"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // M5.3: Race conditions + batch ops stress test (sentinel #44)
    // ─────────────────────────────────────────────────────────────────
    //
    // Three concurrency scenarios sentinel must handle without losing
    // proof entries or corrupting the chain:
    //
    // 1. Many concurrent submissions, DIFFERENT skills, same handler
    //    instance. Each skill gets its own chain so there's no shared
    //    chain to corrupt — but the RwLock around SessionState gates
    //    every write. If the engine grabs the write lock incorrectly
    //    (e.g. locks before validating, or releases between read and
    //    write) entries can be lost.
    //
    // 2. Many concurrent submissions, SAME skill, same handler. Every
    //    write contends for the same chain. The engine must serialize
    //    these so each entry sees a coherent prev_hash. If two
    //    submissions race on prev_hash computation, two entries can
    //    end up with the same prev_hash — a chain fork. The test must
    //    catch that.
    //
    // 3. A "batch" of N submissions sequentially against the same
    //    chain — proves the chain stays linear when not under
    //    concurrent pressure, and exposes leaks in the engine's
    //    bookkeeping that only manifest with depth (e.g. quadratic
    //    state growth that fails at N=100 but not N=10).
    //
    // Tests use the existing handler + state types — no new mocks.

    #[tokio::test]
    async fn m5_3_concurrent_different_skills_seals_all_chains() {
        // 50 skills × 1 submission each, fired in parallel through one
        // shared handler. If the engine has lock-ordering bugs that
        // cause writes to be dropped, we'll lose chains here.
        let state = Arc::new(RwLock::new(SessionState::new("m5-3-multi-skill")));
        let engine = test_engine(state.clone());
        let handler =
            Arc::new(McpHandler::new(state.clone(), engine).with_default_test_workflows());

        let skill_count: usize = 50;
        let mut futures = Vec::with_capacity(skill_count);
        for i in 0..skill_count {
            let h = Arc::clone(&handler);
            futures.push(tokio::spawn(async move {
                let skill = format!("stress_skill_{i:03}");
                record_handler_independent_verdict(&h, &skill, "claim", "0.1", true, 0.9).await;
                let result = h
                    .handle(McpToolCall {
                        name: "sentinel__submit_step_complete".into(),
                        arguments: serde_json::json!({
                            "skill": skill,
                            "phase_id": "claim",
                            "step_id": "0.1",
                            "step_description": format!("concurrent submission for skill {i}"),
                            "verdict": {
                                "sufficient": true,
                                "confidence": 0.9,
                                "reasoning": "stress test",
                            },
                            "started_at": step_started_at(),
                            "evidence": empty_evidence_json(),
                            "artifact": {"index": i},
                        }),
                    })
                    .await;
                (i, result.success)
            }));
        }

        let mut results: Vec<(usize, bool)> = Vec::with_capacity(futures.len());
        for f in futures {
            results.push(f.await.expect("task panicked"));
        }

        // Every single submission must succeed.
        let failures: Vec<usize> = results
            .iter()
            .filter_map(|(i, ok)| if !ok { Some(*i) } else { None })
            .collect();
        assert!(
            failures.is_empty(),
            "{} submissions failed under concurrent load: {:?}",
            failures.len(),
            failures
        );

        // Every skill must end up with exactly one chain entry.
        let s = state.read().await;
        assert_eq!(
            s.proof_chain_count(),
            skill_count,
            "every skill must have its own chain"
        );
        for i in 0..skill_count {
            let key = format!("stress_skill_{i:03}");
            let chain = s.proof_chain(&key).unwrap_or_else(|| {
                panic!("chain for {key} missing — entry lost under concurrent load");
            });
            assert_eq!(
                chain.entries.len(),
                1,
                "chain for {key} has wrong entry count: {}",
                chain.entries.len()
            );
        }
    }

    #[tokio::test]
    async fn m5_3_concurrent_same_skill_does_not_fork_chain() {
        // 30 submissions to the SAME skill, all fired in parallel. The
        // engine must serialize the writes; the final chain must
        // contain exactly 30 entries, with UNIQUE combined_hashes
        // (chain fork → duplicate prev_hash → duplicate combined_hash).
        let state = Arc::new(RwLock::new(SessionState::new("m5-3-same-skill")));
        let engine = test_engine(state.clone());
        let handler =
            Arc::new(McpHandler::new(state.clone(), engine).with_default_test_workflows());

        let submission_count: usize = 30;
        let mut futures = Vec::with_capacity(submission_count);
        for i in 0..submission_count {
            let h = Arc::clone(&handler);
            futures.push(tokio::spawn(async move {
                let step_id = format!("0.{i}");
                record_handler_independent_verdict(&h, "linear", "claim", &step_id, true, 0.9)
                    .await;
                h.handle(McpToolCall {
                    name: "sentinel__submit_step_complete".into(),
                    arguments: serde_json::json!({
                        "skill": "linear",
                        "phase_id": "claim",
                        // Unique step_id per submission so the engine
                        // doesn't reject as duplicate-step. Real-world
                        // concurrent calls would also have distinct
                        // step_ids since each step is a different unit
                        // of work.
                        "step_id": step_id,
                        "step_description": format!("concurrent step {i}"),
                        "verdict": {
                            "sufficient": true,
                            "confidence": 0.9,
                            "reasoning": "stress",
                        },
                        "started_at": step_started_at(),
                        "evidence": empty_evidence_json(),
                        "artifact": {"index": i},
                    }),
                })
                .await
            }));
        }

        let mut results = Vec::with_capacity(futures.len());
        for f in futures {
            results.push(f.await.expect("task panicked"));
        }
        let succeeded = results.iter().filter(|r| r.success).count();
        assert_eq!(
            succeeded, submission_count,
            "all {submission_count} concurrent submissions to one skill must succeed"
        );

        let s = state.read().await;
        let chain = s.proof_chain("linear").expect("chain exists");
        assert_eq!(
            chain.entries.len(),
            submission_count,
            "chain must have exactly one entry per submission"
        );

        // Hash uniqueness across the entire chain.
        let hashes: Vec<&str> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s.combined_hash.as_str()),
                _ => None,
            })
            .collect();
        let unique_hashes: std::collections::HashSet<&str> = hashes.iter().copied().collect();
        assert_eq!(
            unique_hashes.len(),
            hashes.len(),
            "all {} chain entries must have distinct combined_hashes (no fork)",
            hashes.len()
        );

        // prev_hash chain integrity: every entry's prev_hash must match
        // the previous entry's combined_hash, except the first which
        // points at GENESIS_HASH. If two siblings shared a prev_hash,
        // the chain forked and we'd detect it here.
        let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();
        for window in step_entries.windows(2) {
            assert_eq!(
                window[1].previous_hash, window[0].combined_hash,
                "chain must be linear — prev_hash of step n must equal combined_hash of step n-1"
            );
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // BIBLE: evidence-adapter wireup (sentinel #68)
    // ─────────────────────────────────────────────────────────────────
    //
    // submit_step_complete gains an optional `evidence_claim` arg. When
    // present + a registry is wired on the handler, the registry fetches
    // an EvidenceReceipt for the claim and folds it into the sealed
    // proof's `Evidence.custom.evidence_receipt`. Four behaviors to
    // pin down:
    //   1. No claim supplied → no evidence-adapter receipt is attached.
    //   2. Claim supplied + verified adapter wired → receipt lands in
    //      the proof's evidence.
    //   3. Claim supplied + registry NOT wired → fail-fast error.
    //   4. Claim supplied + self-attested/verified=false receipt →
    //      fail-closed before proof sealing.

    #[tokio::test]
    async fn bible_no_evidence_claim_no_registry_works_unchanged() {
        // Pre-#68 callers must still work without any registry.
        let state = Arc::new(RwLock::new(SessionState::new("bible-noop")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();
        record_independent_verdict(&state, "claim", "1", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "fetch ticket",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.9,
                        "reasoning": "ok",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(result.success, "no-claim path failed: {:?}", result.error);
    }

    #[tokio::test]
    async fn bible_evidence_claim_with_registry_folds_receipt_into_evidence() {
        let state = Arc::new(RwLock::new(SessionState::new("bible-receipt")));
        let engine = test_engine(state.clone());
        let registry = registry_with_verified_adapter("github_api", &["git.pr_opened"]);
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_evidence_adapters(registry);
        record_independent_verdict(&state, "review", "3.L3", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "review",
                    "step_id": "3.L3",
                    "step_description": "open PR with Ref FPCRM-100",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.9,
                        "reasoning": "ok",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "evidence_claim": {
                        "skill": "git",
                        "phase_id": "review",
                        "step_id": "3.L3",
                        "claim_type": "git.pr_opened",
                        "context": {
                            "pr_number": 4242,
                            "repo": "firefly-pro/firefly-pro-crm",
                        },
                    },
                }),
            })
            .await;
        assert!(result.success, "wire-in failure: {:?}", result.error);

        // The sealed proof must carry the receipt in evidence.custom.
        let s = state.read().await;
        let chain = s.proof_chain("linear").expect("chain exists");
        let last_step = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .last()
            .expect("at least one step sealed");
        let custom = &last_step.evidence.custom;
        let receipt = custom
            .get("evidence_receipt")
            .expect("evidence_receipt folded into custom");
        assert_eq!(
            receipt.get("adapter_name").and_then(|v| v.as_str()),
            Some("github_api"),
            "verified adapter should have produced the receipt"
        );
        assert_eq!(
            receipt.get("verified").and_then(|v| v.as_bool()),
            Some(true),
            "step proof evidence claims must fold verified receipts only"
        );
    }

    #[tokio::test]
    async fn bible_evidence_claim_self_attested_receipt_fails_closed() {
        let state = Arc::new(RwLock::new(SessionState::new("bible-self-attested-block")));
        let engine = test_engine(state.clone());
        let registry = Arc::new(
            crate::evidence_adapters::EvidenceAdapterRegistry::with_self_attested_adapter(),
        );
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_evidence_adapters(registry);
        record_independent_verdict(&state, "review", "3.L3", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "review",
                    "step_id": "3.L3",
                    "step_description": "open PR with Ref FPCRM-100",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.9,
                        "reasoning": "ok",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "evidence_claim": {
                        "skill": "git",
                        "phase_id": "review",
                        "step_id": "3.L3",
                        "claim_type": "git.pr_opened",
                        "context": {
                            "pr_number": 4242,
                            "repo": "firefly-pro/firefly-pro-crm",
                        },
                    },
                }),
            })
            .await;
        assert!(
            !result.success,
            "self-attested evidence must not seal a step proof"
        );
        let err = result.error.expect("error present");
        assert!(
            err.contains("verified=false") && err.contains("refusing to seal"),
            "error must explain fail-closed verification: {err}"
        );
        assert!(
            state.read().await.proof_chain("linear").is_none(),
            "self-attested evidence must not mutate the proof chain"
        );
    }

    #[tokio::test]
    async fn bible_evidence_claim_without_registry_fails_loudly() {
        // No `with_evidence_adapters` call. Supplying evidence_claim
        // anyway must error — silent skip would defeat the point.
        let state = Arc::new(RwLock::new(SessionState::new("bible-no-registry")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();
        record_independent_verdict(&state, "claim", "1", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "x",
                    "verdict": {
                        "sufficient": true,
                        "confidence": 0.9,
                        "reasoning": "ok",
                    },
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "evidence_claim": {
                        "skill": "git",
                        "phase_id": "claim",
                        "step_id": "1",
                        "claim_type": "git.pr_opened",
                        "context": {"pr_number": 1},
                    },
                }),
            })
            .await;
        assert!(!result.success, "must error when registry unwired");
        let err = result.error.unwrap();
        assert!(
            err.contains("evidence_claim") && err.contains("no evidence-adapter registry"),
            "error must explain the misconfiguration: {err}"
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // #58 / M7.9: Header/context propagation through StepProofs
    // ─────────────────────────────────────────────────────────────────
    //
    // account_context inherits forward through a skill's chain when not
    // explicitly supplied. Same as request headers in HTTP middleware:
    // the value flows along until something overrides it.
    //
    // Behaviors:
    //   1. Explicit account_context on first step → seal carries it.
    //   2. Omitted on follow-up step → inherits from previous step.
    //   3. Explicit override on follow-up step → seal carries override.
    //   4. Explicit null on follow-up step → seal carries None (clears).
    //   5. First step with no context and no prior chain → None.

    #[tokio::test]
    async fn context_propagation_explicit_then_inherited() {
        let state = Arc::new(RwLock::new(SessionState::new("ctx-inherit")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        // Step 1: explicit account_context.
        record_independent_verdict(&state, "claim", "1", true, 0.9).await;
        let r1 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "claim",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "account_context": "firefly-pro",
                }),
            })
            .await;
        assert!(r1.success);
        assert_eq!(
            r1.content.get("account_context").and_then(|v| v.as_str()),
            Some("firefly-pro")
        );

        // Step 2: NO account_context arg → must inherit "firefly-pro".
        record_independent_verdict(&state, "fetch", "1.1", true, 0.9).await;
        let r2 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "fetch",
                    "step_id": "1.1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(r2.success);
        assert_eq!(
            r2.content.get("account_context").and_then(|v| v.as_str()),
            Some("firefly-pro"),
            "step 2 must inherit account_context from step 1"
        );
    }

    #[tokio::test]
    async fn context_propagation_explicit_override_wins() {
        let state = Arc::new(RwLock::new(SessionState::new("ctx-override")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        record_independent_verdict(&state, "claim", "1", true, 0.9).await;
        let _r1 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "claim",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "account_context": "firefly-pro",
                }),
            })
            .await;

        // Step 2: explicit different context — must use the override.
        record_independent_verdict(&state, "fetch", "1.1", true, 0.9).await;
        let r2 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "fetch",
                    "step_id": "1.1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "account_context": "tenant-x",
                }),
            })
            .await;
        assert!(r2.success);
        assert_eq!(
            r2.content.get("account_context").and_then(|v| v.as_str()),
            Some("tenant-x"),
            "explicit override must beat inheritance"
        );
    }

    #[tokio::test]
    async fn context_propagation_explicit_null_clears_inherited() {
        let state = Arc::new(RwLock::new(SessionState::new("ctx-clear")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        record_independent_verdict(&state, "claim", "1", true, 0.9).await;
        let _r1 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "claim",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "account_context": "firefly-pro",
                }),
            })
            .await;

        // Step 2: explicit null — clears the inherited context.
        record_independent_verdict(&state, "fetch", "1.1", true, 0.9).await;
        let r2 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "fetch",
                    "step_id": "1.1",
                    "step_description": "fetch",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "account_context": null,
                }),
            })
            .await;
        assert!(r2.success);
        // Sealed proof has account_context = None when explicitly cleared.
        let s = state.read().await;
        let chain = s.proof_chain("linear").unwrap();
        let last_step = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .last()
            .unwrap();
        assert_eq!(last_step.account_context, None);
    }

    #[tokio::test]
    async fn context_propagation_no_prior_chain_stays_none() {
        // First step ever, no account_context supplied → None (no inheritance source).
        let state = Arc::new(RwLock::new(SessionState::new("ctx-empty")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        record_independent_verdict(&state, "claim", "1", true, 0.9).await;
        let r1 = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "claim",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(r1.success);
        // No account_context in the result because no source to inherit from.
        let s = state.read().await;
        let chain = s.proof_chain("linear").unwrap();
        let first_step = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .next()
            .unwrap();
        assert_eq!(first_step.account_context, None);
    }

    // ─────────────────────────────────────────────────────────────────
    // #71: Browserbase as third-party verifier (step_verifier wireup)
    // ─────────────────────────────────────────────────────────────────
    //
    // step_verifiers list on McpHandler says "the step at
    // (skill, phase_id, step_id) cannot seal without a receipt from
    // the named adapter." Verified after the BIBLE wireup folds the
    // receipt in, so the verifier sees the freshly-attached receipt.
    //
    // Behaviors to pin down:
    //   1. No matching verifier → non-targeted steps are not blocked.
    //   2. Matching verifier + correct receipt folded → seal proceeds.
    //   3. Matching verifier + no receipt → seal blocked with clear error.
    //   4. Matching verifier + wrong-adapter receipt → seal blocked.
    //   5. Matching verifier + verified=false receipt → seal blocked.

    #[tokio::test]
    async fn step_verifier_no_match_no_impact() {
        let state = Arc::new(RwLock::new(SessionState::new("sv-noop")));
        let engine = test_engine(state.clone());
        // A verifier for a different step — won't match the call.
        let req = sentinel_domain::step_verifier::StepVerifierRequirement::new(
            "github",
            "qa-handoff",
            "3.5.5",
            "browserbase",
        );
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_step_verifiers(vec![req]);
        record_independent_verdict(&state, "claim", "1", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear", // different skill — no match
                    "phase_id": "claim",
                    "step_id": "1",
                    "step_description": "x",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(
            result.success,
            "non-matching verifier must not block: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn step_verifier_matches_and_bibled_receipt_satisfies() {
        // BIBLE registry + matching verifier + the BIBLE wireup folds
        // a receipt in → verifier sees it → seal proceeds.
        let state = Arc::new(RwLock::new(SessionState::new("sv-bibled")));
        let engine = test_engine(state.clone());
        let registry = registry_with_verified_adapter("browserbase", &["browserbase.smoke_test"]);
        let req = sentinel_domain::step_verifier::StepVerifierRequirement::new(
            "linear",
            "qa-handoff",
            "3.5.5",
            "browserbase",
        );
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_evidence_adapters(registry)
            .with_step_verifiers(vec![req]);
        record_independent_verdict(&state, "qa-handoff", "3.5.5", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "qa-handoff",
                    "step_id": "3.5.5",
                    "step_description": "post implementation comment with Browserbase Loom link",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                    "evidence_claim": {
                        "skill": "linear",
                        "phase_id": "qa-handoff",
                        "step_id": "3.5.5",
                        "claim_type": "browserbase.smoke_test",
                        "context": {"recording_id": "bb-session-7733"}
                    },
                }),
            })
            .await;
        assert!(
            result.success,
            "verifier should accept the bibled receipt: {:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn step_verifier_blocks_when_no_receipt() {
        // Matching verifier but NO evidence_claim supplied → no receipt
        // folded → verifier fails the seal.
        let state = Arc::new(RwLock::new(SessionState::new("sv-block")));
        let engine = test_engine(state.clone());
        let req = sentinel_domain::step_verifier::StepVerifierRequirement::new(
            "linear",
            "qa-handoff",
            "3.5.5",
            "browserbase",
        );
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_step_verifiers(vec![req]);
        record_independent_verdict(&state, "qa-handoff", "3.5.5", true, 0.9).await;

        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "qa-handoff",
                    "step_id": "3.5.5",
                    "step_description": "x",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": empty_evidence_json(),
                }),
            })
            .await;
        assert!(!result.success, "verifier must block when receipt missing");
        let err = result.error.unwrap();
        assert!(
            err.contains("browserbase") && err.contains("absent"),
            "error must name the missing adapter + the absence: {err}"
        );
    }

    #[tokio::test]
    async fn step_verifier_blocks_on_unverified_receipt() {
        // Receipt present but verified=false in default verified_only=true
        // mode → seal blocked. Production case: smoke test FAILED, the
        // proof chain refuses to seal a downstream "shipping" step.
        let state = Arc::new(RwLock::new(SessionState::new("sv-unverified")));
        let engine = test_engine(state.clone());
        let req = sentinel_domain::step_verifier::StepVerifierRequirement::new(
            "linear",
            "qa-handoff",
            "3.5.5",
            "fake_failing_adapter",
        );
        let handler = McpHandler::new(state.clone(), engine)
            .with_default_test_workflows()
            .with_step_verifiers(vec![req]);
        record_independent_verdict(&state, "qa-handoff", "3.5.5", true, 0.9).await;

        // Pre-build an "evidence" object with a verified=false receipt
        // manually (bypasses BIBLE wireup; lets us simulate "the
        // adapter ran but returned a failing verdict").
        let result = handler
            .handle(McpToolCall {
                name: "sentinel__submit_step_complete".into(),
                arguments: serde_json::json!({
                    "skill": "linear",
                    "phase_id": "qa-handoff",
                    "step_id": "3.5.5",
                    "step_description": "x",
                    "verdict": {"sufficient": true, "confidence": 0.9, "reasoning": "ok"},
                    "started_at": step_started_at(),
                    "evidence": {
                        "tool_calls": [],
                        "tool_results": [],
                        "files_changed": [],
                        "phase_file_read": false,
                        "custom": {
                            "evidence_receipt": {
                                "adapter_name": "fake_failing_adapter",
                                "verified": false,
                                "payload": {"console_errors": 3}
                            }
                        }
                    },
                }),
            })
            .await;
        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.contains("verified=true") && err.contains("fake_failing_adapter"),
            "error must explain verification failure: {err}"
        );
    }

    #[tokio::test]
    async fn m5_3_deep_sequential_batch_stays_linear() {
        // 100 sequential submissions — proves no quadratic blowup, no
        // bookkeeping leaks, and chain stays linear at depth.
        let state = Arc::new(RwLock::new(SessionState::new("m5-3-batch")));
        let engine = test_engine(state.clone());
        let handler = McpHandler::new(state.clone(), engine).with_default_test_workflows();

        let depth: usize = 100;
        for i in 0..depth {
            let phase_id = if i % 2 == 0 { "claim" } else { "fetch" };
            let step_id = format!("{i}");
            record_handler_independent_verdict(&handler, "linear", phase_id, &step_id, true, 0.92)
                .await;
            let result = handler
                .handle(McpToolCall {
                    name: "sentinel__submit_step_complete".into(),
                    arguments: serde_json::json!({
                        "skill": "linear",
                        "phase_id": phase_id,
                        "step_id": step_id,
                        "step_description": format!("batch step {i}"),
                        "verdict": {
                            "sufficient": true,
                            "confidence": 0.92,
                            "reasoning": "batch op",
                        },
                        "started_at": step_started_at(),
                        "evidence": empty_evidence_json(),
                        "artifact": {"index": i},
                    }),
                })
                .await;
            assert!(result.success, "batch step {i} failed: {:?}", result.error);
        }

        let s = state.read().await;
        let chain = s.proof_chain("linear").unwrap();
        assert_eq!(chain.entries.len(), depth);

        let step_entries: Vec<&sentinel_domain::step_proof::StepProof> = chain
            .entries
            .iter()
            .filter_map(|e| match e {
                sentinel_domain::proof::ProofEntry::Step(s) => Some(s),
                _ => None,
            })
            .collect();
        for window in step_entries.windows(2) {
            assert_eq!(
                window[1].previous_hash, window[0].combined_hash,
                "chain must remain linear at depth"
            );
        }

        // Final entry index matches the depth-1, proving nothing got
        // silently re-ordered or dropped.
        let final_step = step_entries.last().unwrap();
        assert_eq!(
            final_step.artifact.get("index").and_then(|v| v.as_u64()),
            Some((depth - 1) as u64)
        );
    }
}
