//! `sentinel mcp` — MCP server over stdio
//!
//! Claude Code connects to this as an MCP server.
//! Reads JSON-RPC requests from stdin, writes responses to stdout.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use sentinel_application::mcp_handler::{
    AggregateGraphAudit, BaDraftGraphPort, BaDraftGraphRequest, BaDraftGraphRun,
    CacheEfficiencyGraphAuditPort, CodeReconciliationAuditPort, CodeReconciliationGraphAudit,
    CostPerPointGraphAuditPort, DeployFrequencyGraphAuditPort, DevScorecardGraphAudit,
    DevScorecardGraphAuditPort, EvalRunGraphAudit, EvalRunGraphPort, EvalRunRequest,
    LinearHealthGraphAudit, LinearHealthGraphAuditPort, McpHandler, McpProofReadGraphAudit,
    McpProofReadGraphAuditPort, McpProofReadSurface as AppMcpProofReadSurface, McpToolCall,
    PmAuditGraphAudit, PmAuditGraphAuditPort, PrReviewGraphAuditPort, RoiGraphAuditPort,
    SeverityGraphAudit, SeverityGraphAuditPort, SlaGraphAuditPort, TokenCostGraphAudit,
    TokenCostGraphAuditPort, TokenUsageGraphAuditPort,
};
use sentinel_application::proof_engine::{
    PhaseGraphApplyResult, PhaseGraphAuthority, ProofEngine, StepGraphApplyResult,
    StepGraphAuthority,
};
use sentinel_domain::evidence::Evidence;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{
    DyadVerdicts, SkillSteps, SkillWorkflow, StepStatus, WorkflowState, WorkflowStep,
};
use sentinel_infrastructure::mcp_transport::{JsonRpcRequest, JsonRpcResponse};
use sentinel_infrastructure::workflow_api_read_graph::WorkflowApiReadSurface;

use crate::phase_graph_projection::{
    graph_checkpoint_projection, graph_history_projection, graph_introspection,
    graph_latest_workflow_state, graph_projected_workflows, graph_writes_projection,
    load_workflow_configs, phase_graph_db_path, project_phase_graph_workflows,
};

fn load_workflow_configs_for_rpc(
    request: &JsonRpcRequest,
) -> std::result::Result<HashMap<String, sentinel_domain::workflow::SkillWorkflow>, JsonRpcResponse>
{
    load_workflow_configs().map_err(|e| {
        JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("workflow config load failed: {e}")),
            ),
        )
    })
}

fn load_step_configs_for_workflows(
    workflows: &HashMap<String, SkillWorkflow>,
) -> Result<HashMap<String, SkillSteps>> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    let mut step_configs = HashMap::new();
    for skill in workflows.keys() {
        let steps = sentinel_infrastructure::config::load_skill_steps(&config_dir, skill)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "configured LangGraph workflow '{skill}' is missing required step config '{}'",
                    config_dir
                        .join("steps")
                        .join(format!("{skill}.toml"))
                        .display()
                )
            })?;
        step_configs.insert(skill.clone(), steps);
    }
    Ok(step_configs)
}

struct CliPhaseGraphAuthority;

#[async_trait::async_trait]
impl PhaseGraphAuthority for CliPhaseGraphAuthority {
    async fn apply_verdict(
        &self,
        skill: &str,
        session_id: &str,
        workflow: &sentinel_domain::workflow::SkillWorkflow,
        phase_id: &str,
        passed: bool,
    ) -> anyhow::Result<PhaseGraphApplyResult> {
        let db_path = phase_graph_db_path(session_id)?;
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let report = graph
            .apply_verdict_report(skill, session_id, phase_id, passed)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut graph_run =
            phase_graph_mutation_evidence(&graph, session_id, &report.state).await?;
        graph_run
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("phase graph evidence must be an object"))?
            .insert("stream".to_string(), serde_json::json!(report.stream));
        Ok(PhaseGraphApplyResult {
            workflow_state: report.state.to_workflow_state(),
            graph_run,
        })
    }
}

#[async_trait::async_trait]
impl StepGraphAuthority for CliPhaseGraphAuthority {
    async fn apply_step_status(
        &self,
        skill: &str,
        session_id: &str,
        workflow: &sentinel_domain::workflow::SkillWorkflow,
        phase_id: &str,
        step_id: &str,
        step_policy: &WorkflowStep,
        status: StepStatus,
        summary: Option<String>,
    ) -> anyhow::Result<StepGraphApplyResult> {
        let db_path = phase_graph_db_path(session_id)?;
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph_state = graph
            .update_step(
                skill,
                session_id,
                phase_id,
                step_id,
                step_policy,
                status,
                summary,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph_run = phase_graph_mutation_evidence(&graph, session_id, &graph_state).await?;
        Ok(StepGraphApplyResult {
            workflow_state: graph_state.to_workflow_state(),
            graph_run,
        })
    }
}

struct CliSeverityGraphAuditor;

#[async_trait::async_trait]
impl SeverityGraphAuditPort for CliSeverityGraphAuditor {
    async fn audit_severity_proposals(
        &self,
        proposals: &[sentinel_application::severity::SeverityProposal],
        graph_jsonl: &Path,
    ) -> anyhow::Result<SeverityGraphAudit> {
        crate::severity_graph_audit::audit_severity_proposals(proposals, graph_jsonl).await
    }
}

struct CliCodeReconciliationAuditor;

struct CliPmAuditGraphAuditor;

#[async_trait::async_trait]
impl PmAuditGraphAuditPort for CliPmAuditGraphAuditor {
    async fn audit_pm_flags(
        &self,
        flags: &[sentinel_application::linear_pm_audit::PmFlag],
        graph_jsonl: &Path,
    ) -> anyhow::Result<PmAuditGraphAudit> {
        crate::pm_audit_graph::run_pm_audit_graph_audit(flags, graph_jsonl).await
    }
}

struct CliLinearHealthGraphAuditor;

#[async_trait::async_trait]
impl LinearHealthGraphAuditPort for CliLinearHealthGraphAuditor {
    async fn audit_linear_health(
        &self,
        summary: &sentinel_application::linear_health_score::HealthSummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<LinearHealthGraphAudit> {
        crate::linear_health_graph::run_linear_health_graph_audit(summary, graph_jsonl).await
    }
}

struct CliDevScorecardGraphAuditor;

#[async_trait::async_trait]
impl DevScorecardGraphAuditPort for CliDevScorecardGraphAuditor {
    async fn audit_dev_scores(
        &self,
        scores: &[sentinel_application::dev_scorecard::DevScore],
        graph_jsonl: &Path,
    ) -> anyhow::Result<DevScorecardGraphAudit> {
        crate::dev_scorecard_graph::run_dev_scorecard_graph_audit(scores, graph_jsonl).await
    }
}

struct CliTokenCostGraphAuditor;

#[async_trait::async_trait]
impl TokenCostGraphAuditPort for CliTokenCostGraphAuditor {
    async fn audit_token_cost(
        &self,
        summary: &sentinel_application::token_cost::TokenCostSummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<TokenCostGraphAudit> {
        crate::token_cost_graph::run_token_cost_graph_audit(summary, graph_jsonl).await
    }
}

struct CliTokenUsageGraphAuditor;

#[async_trait::async_trait]
impl TokenUsageGraphAuditPort for CliTokenUsageGraphAuditor {
    async fn audit_token_usage(
        &self,
        report: &sentinel_application::tokens::ScanReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit =
            crate::token_usage_graph::run_token_usage_graph_audit(report, graph_jsonl).await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

struct CliCacheEfficiencyGraphAuditor;

#[async_trait::async_trait]
impl CacheEfficiencyGraphAuditPort for CliCacheEfficiencyGraphAuditor {
    async fn audit_cache_efficiency(
        &self,
        report: &sentinel_application::cache_efficiency::CacheReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit =
            crate::cache_efficiency_graph::run_cache_efficiency_graph_audit(report, graph_jsonl)
                .await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

struct CliCostPerPointGraphAuditor;

#[async_trait::async_trait]
impl CostPerPointGraphAuditPort for CliCostPerPointGraphAuditor {
    async fn audit_cost_per_point(
        &self,
        report: &sentinel_application::cost_per_point::CostPerPointReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit =
            crate::cost_per_point_graph::run_cost_per_point_graph_audit(report, graph_jsonl)
                .await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

struct CliDeployFrequencyGraphAuditor;

#[async_trait::async_trait]
impl DeployFrequencyGraphAuditPort for CliDeployFrequencyGraphAuditor {
    async fn audit_deploy_frequency(
        &self,
        summary: &sentinel_application::deploy_freq::DeploySummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit =
            crate::deploy_freq_graph::run_deploy_frequency_graph_audit(summary, graph_jsonl)
                .await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

struct CliPrReviewGraphAuditor;

#[async_trait::async_trait]
impl PrReviewGraphAuditPort for CliPrReviewGraphAuditor {
    async fn audit_pr_review(
        &self,
        report: &sentinel_application::pr_review::PrReviewReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit = crate::pr_review_graph::run_pr_review_graph_audit(report, graph_jsonl).await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

struct CliRoiGraphAuditor;

#[async_trait::async_trait]
impl RoiGraphAuditPort for CliRoiGraphAuditor {
    async fn audit_roi(
        &self,
        report: &sentinel_application::roi::RoiReport,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit = crate::roi_graph::run_roi_graph_audit(report, graph_jsonl).await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

struct CliSlaGraphAuditor;

#[async_trait::async_trait]
impl SlaGraphAuditPort for CliSlaGraphAuditor {
    async fn audit_sla(
        &self,
        summary: &sentinel_application::sla::BreachesSummary,
        graph_jsonl: &Path,
    ) -> anyhow::Result<AggregateGraphAudit> {
        let audit = crate::sla_graph::run_sla_graph_audit(summary, graph_jsonl).await?;
        Ok(AggregateGraphAudit {
            workflow_authority: audit.workflow_authority,
            graph: audit.graph,
            graph_runs_path: audit.graph_runs_path,
            decision: audit.decision,
            authorization_checkpoint: audit.authorization_checkpoint,
            thread_id: audit.thread_id,
            run: audit.run,
        })
    }
}

#[async_trait::async_trait]
impl CodeReconciliationAuditPort for CliCodeReconciliationAuditor {
    async fn audit_code_flags(
        &self,
        flags: &[sentinel_application::linear_code_audit::CodeFlag],
        graph_jsonl: &Path,
    ) -> anyhow::Result<CodeReconciliationGraphAudit> {
        crate::code_reconciliation_audit::run_code_reconciliation_graph_audit(flags, graph_jsonl)
            .await
    }
}

struct CliMcpProofReadGraphAuditor;

fn mcp_proof_read_surface(
    surface: AppMcpProofReadSurface,
) -> sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface {
    match surface {
        AppMcpProofReadSurface::ProofChain => {
            sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface::ProofChain
        }
        AppMcpProofReadSurface::WorkflowStatus => {
            sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface::WorkflowStatus
        }
        AppMcpProofReadSurface::VerifyChain => {
            sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface::VerifyChain
        }
        AppMcpProofReadSurface::StepProof => {
            sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface::StepProof
        }
        AppMcpProofReadSurface::StepChain => {
            sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface::StepChain
        }
        AppMcpProofReadSurface::ActiveStep => {
            sentinel_infrastructure::mcp_proof_read_graph::McpProofReadSurface::ActiveStep
        }
    }
}

#[async_trait::async_trait]
impl McpProofReadGraphAuditPort for CliMcpProofReadGraphAuditor {
    async fn audit_mcp_proof_read(
        &self,
        surface: AppMcpProofReadSurface,
        response: &serde_json::Value,
        graph_jsonl: &Path,
    ) -> anyhow::Result<McpProofReadGraphAudit> {
        let graph_surface = mcp_proof_read_surface(surface);
        let response_hash = sentinel_infrastructure::mcp_proof_read_graph::sha256_json(response);
        let identifier = sentinel_infrastructure::mcp_proof_read_graph::mcp_proof_read_identifier(
            graph_surface,
            response,
            &response_hash,
        )
        .map_err(|e| anyhow::anyhow!("build MCP proof read graph identifier: {e}"))?;
        let state = sentinel_infrastructure::mcp_proof_read_graph::McpProofReadState::from_response(
            graph_surface,
            identifier,
            response,
        );
        let graph = sentinel_infrastructure::mcp_proof_read_graph::build_mcp_proof_read_graph()
            .await
            .map_err(|e| anyhow::anyhow!("build MCP proof read graph: {e}"))?;
        let run =
            sentinel_infrastructure::mcp_proof_read_graph::run_mcp_proof_read_decision_report(
                &graph, state,
            )
            .await
            .map_err(|e| anyhow::anyhow!("run MCP proof read graph: {e}"))?;
        let authorization = run
            .mcp_proof_read_authorization()
            .map_err(|e| anyhow::anyhow!("MCP proof read graph authorization failed: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!("MCP proof read graph produced no terminal checkpoint")
            })?;
        let decision = sentinel_infrastructure::mcp_proof_read_graph::mcp_proof_read_decision_label(
            authorization.decision(),
        );
        let authorization_checkpoint = authorization.checkpoint_ref();
        let thread_id = run.thread_id.clone();
        if let Some(parent) = graph_jsonl.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create MCP proof read graph dir {}", parent.display()))?;
        }
        let row = serde_json::json!({
            "workflow_authority": "langgraph",
            "graph": "mcp_proof_read",
            "surface": surface.label(),
            "response_sha256": response_hash.clone(),
            "decision": decision,
            "authorization_checkpoint": authorization_checkpoint.clone(),
            "thread_id": thread_id.clone(),
            "run": run,
        });
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(graph_jsonl)
            .with_context(|| {
                format!("open MCP proof read graph audit {}", graph_jsonl.display())
            })?;
        serde_json::to_writer(&mut file, &row).with_context(|| {
            format!("write MCP proof read graph audit {}", graph_jsonl.display())
        })?;
        file.write_all(b"\n").with_context(|| {
            format!(
                "terminate MCP proof read graph audit {}",
                graph_jsonl.display()
            )
        })?;
        Ok(McpProofReadGraphAudit {
            workflow_authority: "langgraph",
            graph: "mcp_proof_read",
            surface: surface.label(),
            graph_runs_path: graph_jsonl.to_path_buf(),
            response_sha256: response_hash,
            decision: decision.to_string(),
            authorization_checkpoint,
            thread_id,
            run: row["run"].clone(),
        })
    }
}

struct CliEvalRunGraphRunner;

#[async_trait::async_trait]
impl EvalRunGraphPort for CliEvalRunGraphRunner {
    async fn run_eval(&self, request: EvalRunRequest) -> anyhow::Result<EvalRunGraphAudit> {
        let corpus = request.corpus_dir.map_or_else(
            sentinel_infrastructure::eval_corpus::FilesystemEvalCorpus::with_default_path,
            |dir| Ok(sentinel_infrastructure::eval_corpus::FilesystemEvalCorpus::at_dir(dir)),
        )?;
        let store = request.runs_dir.map_or_else(
            sentinel_infrastructure::eval_run_store::FilesystemEvalRunStore::with_default_path,
            |dir| Ok(sentinel_infrastructure::eval_run_store::FilesystemEvalRunStore::at_dir(dir)),
        )?;
        let scorer = sentinel_infrastructure::eval_scorer::LlmEvalScorer::from_env()
            .context("failed to build eval scorer from environment")?;
        let args = crate::eval_cmd::RunArgs {
            run_id: request.run_id,
            candidates_path: request.candidates_path.to_string_lossy().into_owned(),
            case_ids: request.case_ids,
            corpus_dir: None,
            runs_dir: None,
            json: true,
        };
        let run = crate::eval_cmd::run_with(&args, &corpus, &store, &scorer)?;
        let graph_jsonl = store
            .base_dir()
            .join(format!("{}.graph-runs.jsonl", run.run_id.as_str()));
        let audit = crate::eval_graph::run_eval_graph_audit(&run, &graph_jsonl).await?;
        Ok(EvalRunGraphAudit {
            workflow_authority: "langgraph",
            run,
            graph_audit: AggregateGraphAudit {
                workflow_authority: audit.workflow_authority,
                graph: audit.graph,
                graph_runs_path: audit.graph_runs_path,
                decision: audit.decision,
                authorization_checkpoint: audit.authorization_checkpoint,
                thread_id: audit.thread_id,
                run: audit.run,
            },
        })
    }
}

struct CliBaDraftGraphRunner;

#[async_trait::async_trait]
impl BaDraftGraphPort for CliBaDraftGraphRunner {
    async fn draft_ba_recommendation(
        &self,
        request: BaDraftGraphRequest,
    ) -> anyhow::Result<BaDraftGraphRun> {
        let llm = sentinel_infrastructure::openrouter_llm::OpenRouterLlm::from_env()
            .context("failed to build OpenRouter LLM from environment")?;
        let result = crate::ba_cmd::draft_result_with(
            crate::ba_cmd::DraftArgs {
                brief: request.brief,
                audience: request.audience,
                constraints: request.constraints,
                agent_id: request.agent_id,
                json: true,
            },
            &llm,
        )
        .await?;
        Ok(BaDraftGraphRun {
            workflow_authority: "langgraph",
            recommendation: result.recommendation,
            graph_audit: AggregateGraphAudit {
                workflow_authority: result.graph_audit.workflow_authority,
                graph: result.graph_audit.graph,
                graph_runs_path: result.graph_audit.graph_runs_path,
                decision: result.graph_audit.decision,
                authorization_checkpoint: result.graph_audit.authorization_checkpoint,
                thread_id: result.graph_audit.thread_id,
                run: result.graph_audit.run,
            },
        })
    }
}

async fn phase_graph_mutation_evidence(
    graph: &sentinel_graph::CompiledPhaseGraph,
    session_id: &str,
    state: &sentinel_graph::PhaseGraphState,
) -> anyhow::Result<serde_json::Value> {
    let checkpoints = graph
        .phase_snapshots(session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let writes = graph
        .phase_writes_history(session_id, None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let topology = graph
        .introspect(session_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let expected_thread_id = graph
        .thread_id_for_session(session_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut evidence = build_phase_graph_mutation_evidence(
        session_id,
        state,
        checkpoints,
        writes,
        &expected_thread_id,
    )?;
    evidence
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("phase graph evidence must be an object"))?
        .insert("graph_topology".to_string(), serde_json::json!(topology));
    Ok(evidence)
}

fn build_phase_graph_mutation_evidence(
    session_id: &str,
    state: &sentinel_graph::PhaseGraphState,
    checkpoints: Vec<sentinel_graph::PhaseGraphCheckpointSnapshot>,
    writes: Vec<sentinel_graph::PhaseGraphWriteHistoryEntry>,
    expected_thread_id: &str,
) -> anyhow::Result<serde_json::Value> {
    let latest_checkpoint = checkpoints
        .last()
        .ok_or_else(|| anyhow::anyhow!("phase graph mutation did not persist a checkpoint"))?;
    if latest_checkpoint.thread_id != expected_thread_id {
        anyhow::bail!(
            "phase graph latest checkpoint thread mismatch for session '{session_id}': expected '{expected_thread_id}', got '{}'",
            latest_checkpoint.thread_id
        );
    }
    if let Some(mismatched) = checkpoints
        .iter()
        .find(|checkpoint| checkpoint.thread_id != expected_thread_id)
    {
        anyhow::bail!(
            "phase graph checkpoint history for session '{session_id}' contains thread '{}', expected '{expected_thread_id}'",
            mismatched.thread_id
        );
    }
    for pair in checkpoints.windows(2) {
        if pair[0].step_number > pair[1].step_number {
            anyhow::bail!(
                "phase graph checkpoint history for session '{session_id}' is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                pair[0].checkpoint_id,
                pair[0].step_number,
                pair[1].checkpoint_id,
                pair[1].step_number
            );
        }
    }
    if let Some(mismatched) = writes
        .iter()
        .find(|write| write.thread_id != expected_thread_id)
    {
        anyhow::bail!(
            "phase graph write history for session '{session_id}' contains thread '{}', expected '{expected_thread_id}'",
            mismatched.thread_id
        );
    }
    for pair in writes.windows(2) {
        if pair[0].step_number > pair[1].step_number {
            anyhow::bail!(
                "phase graph write history for session '{session_id}' is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                pair[0].checkpoint_id,
                pair[0].step_number,
                pair[1].checkpoint_id,
                pair[1].step_number
            );
        }
    }
    if latest_checkpoint.state != *state {
        anyhow::bail!("phase graph latest checkpoint state mismatch for session '{session_id}'");
    }
    if latest_checkpoint
        .writes
        .iter()
        .all(|write| write.channel != "state")
    {
        anyhow::bail!(
            "phase graph latest checkpoint for session '{session_id}' omitted state-channel checkpoint metadata"
        );
    }
    let state_json = serde_json::to_value(state)?;
    let latest_state_write = writes.iter().any(|write| {
        write.checkpoint_id == latest_checkpoint.checkpoint_id
            && write.channel == "state"
            && write.value_json == state_json
    });
    if !latest_state_write {
        anyhow::bail!(
            "phase graph write history for session '{session_id}' omitted latest checkpoint state-channel write"
        );
    }
    let latest_checkpoint = latest_checkpoint.clone();
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph_state": state,
        "state": state,
        "latest_checkpoint": latest_checkpoint,
        "checkpoints": checkpoints,
        "writes": writes,
    }))
}

// ── Session id detection ────────────────────────────────────────────
//
// The MCP server is a long-lived process that outlives any single Claude
// Code session — one sentinel-mcp.exe is shared across every session on
// the machine. Pinning a `session_id` at process startup (the old
// design) meant `get_session_stats` etc. reported stale state from
// whichever session happened to launch the server first.
//
// Single source of truth: the live transcript files Claude Code writes
// at `~/.claude/projects/{project-key}/{session-id}.jsonl`. The filename
// stem IS the session id; Claude Code appends a line to the transcript
// on every assistant message and tool call, so mtime tracks activity
// tighter than sentinel's own state dir (which only updates on hook
// firings). Resolve the live session by scanning for the newest-mtime
// `.jsonl` under `~/.claude/projects/`.
//
// We do this per-request, not per-process, so a long-running MCP daemon
// self-corrects as the user starts new Claude Code sessions.

/// Walk `~/.claude/projects/*/*.jsonl` and return the filename stem of
/// the most-recently-modified transcript. That stem IS the session id
/// (UUID-shaped).
///
/// Returns `None` if no transcripts exist — in that case the caller
/// should surface an explicit "no active session" error rather than
/// fabricating a timestamped id that won't match any real state.
/// Load the required Ed25519 signing key from `SENTINEL_SIGNING_KEY`
/// (32-byte hex seed). MCP proof sealing is audit-grade by default: malformed
/// or missing key material is a startup error, not an unsigned-proof fallback.
fn load_signing_key_from_env() -> Result<ed25519_dalek::SigningKey> {
    let raw = std::env::var("SENTINEL_SIGNING_KEY").context(
        "SENTINEL_SIGNING_KEY is required for sentinel MCP proof sealing \
         (32-byte hex Ed25519 seed)",
    )?;
    let bytes = hex::decode(raw.trim())
        .context("SENTINEL_SIGNING_KEY must be valid hex for a 32-byte Ed25519 seed")?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "SENTINEL_SIGNING_KEY must decode to exactly 32 bytes; got {}",
            bytes.len()
        );
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&bytes);
    Ok(ed25519_dalek::SigningKey::from_bytes(&seed))
}

/// Load the Ed25519 PUBLIC verifying key from `SENTINEL_VERIFY_KEY` (32-byte
/// hex). Deliberately the public key, NOT derived from `SENTINEL_SIGNING_KEY`:
/// deriving it would let whoever holds the signing key re-sign a forged chain.
/// Missing or malformed verifier material is an error because enterprise proof
/// reads must not silently downgrade to hash-only verification.
pub(crate) fn load_verify_key_from_env() -> Result<ed25519_dalek::VerifyingKey> {
    let raw = std::env::var("SENTINEL_VERIFY_KEY").context(
        "SENTINEL_VERIFY_KEY is required for Sentinel proof signature verification \
         (32-byte hex Ed25519 public key)",
    )?;
    let bytes = hex::decode(raw.trim())
        .context("SENTINEL_VERIFY_KEY must be valid hex for a 32-byte Ed25519 public key")?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "SENTINEL_VERIFY_KEY must decode to exactly 32 bytes; got {}",
            bytes.len()
        );
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    ed25519_dalek::VerifyingKey::from_bytes(&arr)
        .map_err(|e| anyhow::anyhow!("SENTINEL_VERIFY_KEY is not a valid Ed25519 public key: {e}"))
}

fn detect_live_session_id() -> Option<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let projects = PathBuf::from(home).join(".claude").join("projects");
    if !projects.exists() {
        return None;
    }
    let mut newest: Option<(SystemTime, String)> = None;
    let project_dirs = std::fs::read_dir(&projects).ok()?;
    for project in project_dirs.flatten() {
        let Ok(jsonl_entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in jsonl_entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Accept only UUID-shaped stems (8-4-4-4-12 = 36 chars, 4 hyphens).
            if stem.len() != 36 || stem.matches('-').count() != 4 {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if newest.as_ref().is_none_or(|(t, _)| mtime > *t) {
                newest = Some((mtime, stem.to_string()));
            }
        }
    }
    newest.map(|(_, id)| id)
}

/// Look up the transcript that recorded a specific `toolUseId`. Claude Code
/// tags every `H.callTool({..., _meta: {"claudecode/toolUseId": j}, ...})`
/// call with a unique id; that id also appears as the `id` of the assistant
/// `tool_use` block in the session's transcript JSONL. Finding the
/// transcript where a given toolUseId is the LATEST `tool_use` gives us the
/// specific session that issued the MCP call — even when multiple Claude
/// Code windows are open concurrently.
///
/// Strategy: check the newest-mtime transcript first (covers 99%+ of cases
/// since the tool call just happened). If not found there, scan all transcripts.
///
/// Returns `None` if no transcript contains the id — the caller uses the
/// newest-mtime session in that case because the id may be too fresh for the
/// transcript writer to have flushed, though this race is vanishingly rare since
/// Claude Code flushes after each message.
fn session_id_by_tool_use_id(tool_use_id: &str) -> Option<String> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let projects = PathBuf::from(home).join(".claude").join("projects");
    if !projects.exists() {
        return None;
    }

    // Collect all valid transcript paths with mtime, sort newest-first.
    let mut transcripts: Vec<(SystemTime, PathBuf, String)> = Vec::new();
    let Ok(project_dirs) = std::fs::read_dir(&projects) else {
        return None;
    };
    for project in project_dirs.flatten() {
        let Ok(jsonl_entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for entry in jsonl_entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if stem.len() != 36 || stem.matches('-').count() != 4 {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            transcripts.push((mtime, path, stem));
        }
    }
    transcripts.sort_by_key(|b| std::cmp::Reverse(b.0)); // newest first

    for (_, path, session_id) in transcripts {
        if transcript_contains_tool_use_id(&path, tool_use_id) {
            return Some(session_id);
        }
    }
    None
}

/// Scan a single transcript JSONL for an assistant `tool_use` whose `id`
/// matches the given `tool_use_id`. Reads the file fully into memory and
/// scans lines in reverse order because `tool_use` ids are overwhelmingly at
/// the tail.
fn transcript_contains_tool_use_id(transcript: &Path, tool_use_id: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(transcript) else {
        return false;
    };
    for line in content.lines().rev() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }
            if block.get("id").and_then(|v| v.as_str()) == Some(tool_use_id) {
                return true;
            }
        }
    }
    false
}

/// Resolve the session id for an incoming JSON-RPC request.
///
/// Resolution order:
///   1. `params._meta["claudecode/toolUseId"]` → cross-reference against
///      transcript JSONLs. Disambiguates when multiple Claude Code windows
///      are open and is authoritative when present.
///   2. Newest-mtime transcript under `~/.claude/projects/`. Used only for
///      requests without a toolUseId (e.g. `initialize`, `ping`, internal
///      calls).
///
/// Returns an error if neither source yields a session id, so callers
/// can surface an explicit "no active Claude Code session" rather than
/// silently operating on a fabricated id.
fn resolve_session_id(params: &serde_json::Value) -> Result<String> {
    // 1. Prefer toolUseId lookup — unambiguous across concurrent sessions.
    if let Some(tool_use_id) = params
        .get("_meta")
        .and_then(|m| m.get("claudecode/toolUseId"))
        .and_then(|v| v.as_str())
    {
        if let Some(sid) = session_id_by_tool_use_id(tool_use_id) {
            debug!(tool_use_id, session_id = %sid, "Resolved session via toolUseId");
            return Ok(sid);
        }
        anyhow::bail!(
            "toolUseId `{tool_use_id}` was not found in any Claude Code transcript; refusing to guess a session"
        );
    }

    // 2. Requests without toolUseId use the newest live transcript.
    detect_live_session_id().context(
        "no active Claude Code session found — no transcripts under \
         ~/.claude/projects/. MCP tools require a running Claude Code session.",
    )
}

/// Perform one load-mutate-save transaction against the session state on
/// disk, under an exclusive file lock.
///
/// The same `Arc<RwLock<SessionState>>` is reused across calls to satisfy
/// existing handler signatures (`McpHandler` and friends hold it by Arc).
/// Its contents are OVERWRITTEN at the start of each transaction and
/// saved back at the end, so no stale in-memory state survives between
/// calls. This keeps handlers oblivious to the per-call session
/// resolution while guaranteeing single-writer semantics via the file
/// lock.
///
/// Ordering: file lock → overwrite in-memory state → run handler →
/// save to disk → drop lock. Other processes (hooks, parallel MCP
/// calls) block on the file lock until we drop it, so there's no
/// window for a torn read or a lost update.
async fn with_session_state<F, Fut, R>(
    session_id: &str,
    state_handle: &Arc<RwLock<SessionState>>,
    workflow_configs: &HashMap<String, SkillWorkflow>,
    handler_fn: F,
) -> Result<R>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = R>,
{
    // Acquire the exclusive per-session file lock. `acquire_session_lock`
    // returns a `std::fs::File` whose fd holds the OS-level lock; dropping
    // it releases the lock. We run it via spawn_blocking because it can
    // wait on file I/O, and we hold the lock across the handler's await
    // points without blocking the reactor (the only blocking calls are
    // load/save, which are fast file ops — async wrapping would just
    // add noise).
    let session_id_owned = session_id.to_string();
    let _lock = tokio::task::spawn_blocking(move || {
        sentinel_infrastructure::state_store::acquire_session_lock(&session_id_owned)
    })
    .await
    .context("session lock task panicked")?
    .context("failed to acquire session lock")?;

    // Load the current state from disk. If nothing persisted, seed a
    // fresh SessionState for this session.
    let mut loaded = match sentinel_infrastructure::state_store::load(session_id) {
        Ok(Some(s)) => s,
        Ok(None) => SessionState::new(session_id),
        Err(e) => {
            return Err(e).context("state_store::load failed");
        }
    };

    project_phase_graph_workflows(&mut loaded, workflow_configs)
        .await
        .context("phase graph checkpoint projection failed")?;

    // Install the loaded state into the shared Arc. Handlers see exactly
    // the on-disk state for this session; the Arc itself is just a
    // transport for existing handler signatures.
    {
        let mut guard = state_handle.write().await;
        *guard = loaded;
    }

    // Run the handler.
    let response = handler_fn().await;

    // Save the mutated state back under the same lock.
    {
        let mut guard = state_handle.write().await;
        if let Err(e) = sentinel_infrastructure::state_store::save(&mut guard) {
            error!(session_id, error = %e, "Failed to save session state");
        }
    }

    // Lock drops here, releasing it for other callers.
    Ok(response)
}

/// MCP tool definitions — what we advertise to Claude Code
fn tool_definitions() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            {
                "name": "sentinel__get_proof_chain",
                "description": "Get the cryptographic proof chain for a skill execution. Returns all phase proofs with tessera hashes, evidence, and judge verdicts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__get_workflow_status",
                "description": "Get the current workflow state for a skill. Shows which phases are completed, current phase, and what's next.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__verify_chain",
                "description": "Re-verify the integrity of a skill's proof chain. Checks all hashes are consistent and no tampering has occurred.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name to verify"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__submit_phase_complete",
                "description": "Notify sentinel that a skill phase has been completed. Sentinel will evaluate the evidence and add a proof to the chain if sufficient.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase ID (e.g., 'claim', 'fetch', 'review')"
                        },
                        "summary": {
                            "type": "string",
                            "description": "Brief summary of what was done in this phase"
                        },
                        "started_at": {
                            "type": "string",
                            "description": "Required RFC3339 timestamp from the LangGraph/tool authority path for when the phase started"
                        }
                    },
                    "required": ["skill", "phase_id", "summary", "started_at"]
                }
            },
            {
                "name": "sentinel__record_dyad_verdict",
                "description": "Record role-dyad phase authorization through the durable LangGraph checkpoint timeline.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase ID declaring required_dyad"
                        },
                        "role": {
                            "type": "string",
                            "enum": ["implementer", "reviewer", "tester"],
                            "description": "Dyad role to record. reviewer/tester record a passing sub-verdict."
                        },
                        "agent": {
                            "type": "string",
                            "description": "Stable agent identity that produced the work or dyad verdict"
                        }
                    },
                    "required": ["skill", "phase_id", "role", "agent"]
                }
            },
            {
                "name": "sentinel__get_session_stats",
                "description": "Get execution statistics for the current session — hook invocations, blocked calls, per-hook timing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "sentinel__update_step",
                "description": "Update a non-terminal step status within a skill phase. Completion must use sentinel__submit_step_complete so the StepProof and LangGraph checkpoint are sealed together.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase ID (e.g., 'claim', 'fetch')"
                        },
                        "step_id": {
                            "type": "string",
                            "description": "Step ID (e.g., '0.1', '3.L2.3')"
                        },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "blocked", "in_progress"],
                            "description": "New non-terminal status for the step"
                        },
                        "summary": {
                            "type": "string",
                            "description": "Brief summary of what was done (optional)"
                        }
                    },
                    "required": ["skill", "phase_id", "step_id", "status"]
                }
            },
            {
                "name": "sentinel__submit_step_complete",
                "description": "Seal a completed step by committing both its StepProof and durable LangGraph checkpoint evidence.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase ID (e.g., 'claim', 'fetch')"
                        },
                        "step_id": {
                            "type": "string",
                            "description": "Step ID (e.g., '0.1', '3.L2.3')"
                        },
                        "step_description": {
                            "type": "string",
                            "description": "What sufficient completion means for this step"
                        },
                        "verdict": {
                            "type": "object",
                            "description": "Independent step_judge verdict. Caller-supplied self-certification is rejected.",
                            "properties": {
                                "sufficient": {"type": "boolean"},
                                "confidence": {"type": "number"},
                                "reasoning": {"type": "string"},
                                "requested_evidence": {"type": ["string", "null"]}
                            },
                            "required": ["sufficient", "confidence", "reasoning"]
                        },
                        "summary": {
                            "type": "string",
                            "description": "Brief summary stored in LangGraph step state"
                        },
                        "evidence": {
                            "type": "object",
                            "description": "Structured evidence payload for the StepProof"
                        },
                        "artifact": {
                            "description": "Any JSON artifact sealed into the StepProof"
                        },
                        "account_context": {
                            "type": ["string", "null"],
                            "description": "Optional account/context header for the StepProof"
                        },
                        "started_at": {
                            "type": "string",
                            "description": "Required RFC3339 timestamp from the LangGraph/tool authority path for when the step started"
                        },
                        "evidence_claim": {
                            "type": "object",
                            "description": "Optional third-party evidence-adapter claim"
                        }
                    },
                    "required": ["skill", "phase_id", "step_id", "step_description", "verdict", "evidence", "started_at"]
                }
            },
            {
                "name": "sentinel__get_phase_steps",
                "description": "Get all steps and their status for a specific phase. Shows step descriptions from config and current execution status.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase ID (e.g., 'claim', 'review')"
                        }
                    },
                    "required": ["skill", "phase_id"]
                }
            },
            {
                "name": "sentinel__get_workflow_progress",
                "description": "Get full hierarchical progress for a skill workflow. Shows phase-level and step-level completion across the entire workflow.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        }
                    },
                    "required": ["skill"]
                }
            },
            {
                "name": "sentinel__replay_phase",
                "description": "Time-travel: re-attempt a workflow phase by forking the phase graph from the checkpoint just before that phase was last completed. Drops the target phase (and later) from the completed set so it is re-run and re-judged fresh.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name (e.g., 'linear')"
                        },
                        "phase_id": {
                            "type": "string",
                            "description": "Phase id to replay (e.g., 'review')"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Non-empty audit reason for forking graph history"
                        }
                    },
                    "required": ["skill", "phase_id", "reason"]
                }
            },
            {
                "name": "sentinel__regenerate_claude_md",
                "description": "Regenerate ~/.claude/CLAUDE.md from the compiled template. Re-counts components, refreshes project list and Linear accounts. Takes no arguments.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "sentinel__edit_claude_md_template",
                "description": "Find-and-replace on the CLAUDE.md template source (session_init.rs), then auto-regenerate the live mirror. `find` must appear exactly once in the template — the tool refuses ambiguous or missing substrings. Requires a rebuild + `sentinel stage` for the compiled template to pick up the change.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "find": {
                            "type": "string",
                            "description": "Unique substring to replace in the template source"
                        },
                        "replace": {
                            "type": "string",
                            "description": "Replacement text"
                        }
                    },
                    "required": ["find", "replace"]
                }
            },
            {
                "name": "sentinel__restart_all_mcps",
                "description": "Touch every mcp-router-wrapped MCP binary registered in ~/.claude.json so mcp-router's file watcher triggers a mass restart. Returns a per-server touched/skipped list.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "sentinel__get_wip_snapshot",
                "description": "Get the current WIP-by-stage snapshot — count of in-flight Linear tickets per team per workflow state, plus any active bottleneck flags (review_clog, qa_ceiling). Reads ~/.claude/sentinel/state/wip-snapshot.json populated by the 5-min poller. Returns {captured_at, total_wip, teams: {<key>: {<state>: count}}, bottlenecks: [...]} or {captured_at: null, message: 'no snapshot captured yet'} when the poller hasn't run. Takes no arguments.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "required": []
                }
            },
            {
                "name": "sentinel__route_capability",
                "description": "Query the A2 capability router (per docs/a2-capability-aware-routing.md) to pick the best-fit agent for a unit of work, given its capability requirements. Loads agent profiles from the shipped agents-defaults.toml + optional operator overrides at ~/.claude/sentinel/config/agents.toml. Returns the full RoutingExplanation: chosen AgentId (or null when no agent satisfies or the route remains ambiguous), candidate set, eliminated agents with reasons, fired tie-breakers, and the requirement signature. Used by Legatus AI orchestrators and any external orchestrator that needs to make the same dispatch decision sentinel's hooks make internally — keeps the routing substrate single-source-of-truth across the AI-factory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "requirement": {
                            "type": "object",
                            "description": "CapabilityRequirement JSON per crates/sentinel-domain/src/capability.rs. Shape: {\"required\": [<Capability>...], \"preferred\": [<Capability>...], \"forbidden\": [<Capability>...]}. Capability variants are externally-tagged: {\"Reasoning\": \"deep\"} | {\"DifferentVendorFrom\": \"Anthropic\"} | {\"StructuredOutput\": \"AuditorVerdict\"} | {\"CostBudget\": 0.05} | etc. See docs/a2-capability-aware-routing.md §2 for the full vocabulary.",
                            "properties": {
                                "required": {"type": "array"},
                                "preferred": {"type": "array"},
                                "forbidden": {"type": "array"}
                            },
                            "required": ["required"]
                        }
                    },
                    "required": ["requirement"]
                }
            },
            {
                "name": "sentinel__delegate_codex",
                "description": "Delegate a focused adversarial/code-reasoning task to the Codex worker model (openai/gpt-5.5-pro) via OpenRouter — the same gateway the judge uses. Use for 'poke holes in this approach', 'review this diff for bugs/edge cases', 'is this design sound'. Returns the worker's concrete critique. Requires OPENROUTER_API_KEY.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "What to reason about adversarially (the question / claim / approach to critique)."
                        },
                        "context": {
                            "type": "string",
                            "description": "Optional supporting material (a diff, code, design notes) the worker reads."
                        },
                        "max_tokens": {
                            "type": "integer",
                            "description": "Optional response token cap (default 2048)."
                        }
                    },
                    "required": ["task"]
                }
            },
            {
                "name": "sentinel__delegate_kimi_context_scan",
                "description": "Delegate a cheap large-context scan to the Kimi worker model (moonshotai/kimi-k2.6) via OpenRouter. Answers a specific question against a (potentially large) blob of content, extracting only the relevant facts. Use to offload 'scan this and tell me X' reads from the orchestrator's context. Requires OPENROUTER_API_KEY.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The specific question to answer from the content."
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to scan (file contents, logs, a large blob)."
                        },
                        "max_tokens": {
                            "type": "integer",
                            "description": "Optional response token cap (default 2048)."
                        }
                    },
                    "required": ["question", "content"]
                }
            },
            {
                "name": "sentinel__ba_draft",
                "description": "Draft a BA recommendation from a stakeholder brief through the standardized OpenRouter orchestrator, then validate and authorize the emitted BA1/BA3/A13 envelope through the durable BA draft LangGraph. Returns the recommendation plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/ba-draft/{recommendation_id}.graph-runs.jsonl. Requires OPENROUTER_API_KEY.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "brief": {
                            "type": "string",
                            "description": "Stakeholder brief for the recommendation."
                        },
                        "audience": {
                            "type": "string",
                            "enum": ["exec", "board", "customer", "internal_team"],
                            "description": "Target audience for tone, risk class, and stakeholder fit."
                        },
                        "constraints": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional operator constraints the draft must satisfy or explicitly surface."
                        },
                        "agent_id": {
                            "type": "string",
                            "description": "Optional agent identity for audit attribution. Defaults to ba-orchestrator."
                        }
                    },
                    "required": ["brief", "audience"]
                }
            },
            {
                "name": "sentinel__linear_pm_audit",
                "description": "Run the Linear PM-enforcement audit over the local Linear issue cache (~/.claude/sentinel/linear-assigned.json): estimate hygiene, oversized open tickets, blocked/untracked open work, QA-failed risk, optional velocity burndown, and estimate-vs-actual calibration. Every PM-discipline flag is classified through the durable PM audit LangGraph and returned with checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/linear-pm-audit.{json,jsonl,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "velocity_pts_per_week": {
                            "type": "number",
                            "description": "Measured team velocity (story points per week). Combined with weeks_available, enables the burndown projection."
                        },
                        "weeks_available": {
                            "type": "number",
                            "description": "Weeks remaining until the target date. Combined with velocity_pts_per_week, enables the burndown projection."
                        }
                    }
                }
            },
            {
                "name": "sentinel__severity_scan",
                "description": "LLM-judge each cached Linear ticket's severity/priority (1=urgent .. 4=low) from its title+description, running BOTH Opus 4.8 and GPT-5.5 and reconciling (on disagreement, the more-urgent verdict wins). Classifies each ticket as `set` (no current priority — a gap-fill candidate), `suggest` (priority exists but the proposal differs), or `agree`, then runs every proposal through the durable severity LangGraph and returns checkpoint/write/stream/topology audit evidence. MCP remains read-only: it never mutates Linear, and it writes ~/.claude/sentinel/metrics/severity.{json,jsonl,graph-runs.jsonl}. Requires OPENROUTER_API_KEY to be set when the MCP server started.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__dev_scorecard",
                "description": "Compute per-developer scorecards from ~/.claude/sentinel/dev-git-stats.json joined with the Linear cache, then classify every developer row through the durable developer scorecard LangGraph. Returns the summary plus per-dev checkpoint/write/stream/topology audit evidence, including attribution-divergence authorization. Writes ~/.claude/sentinel/metrics/dev-scorecard.{json,jsonl,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__linear_code_audit",
                "description": "Cross-check every Completed ticket in the Linear cache against a precomputed code-evidence map at ~/.claude/sentinel/ticket-code-evidence.json (ticket -> {commits, files}). A Completed ticket with zero commits AND zero touched files (or no entry at all) is flagged 'done-no-evidence', then every flag is run through the durable reconciliation LangGraph and returned with checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/linear-code-audit.{json,jsonl,reconciliation-graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__linear_health",
                "description": "Compute a composite 0-100 Linear health score over the cache across hygiene, structure, data_quality, and flow dimensions, then validate and authorize the board-level health verdict through the durable Linear health LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/linear-health.{json,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__tokens_scan",
                "description": "Aggregate Claude Code session token usage by Linear ticket, then validate and classify the attribution report through the durable token usage LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/tokens-per-ticket.{jsonl,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__cache_efficiency",
                "description": "Scan prompt-cache hit rates across Claude Code sessions, then validate and classify cache health through the durable cache-efficiency LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/cache-efficiency.{jsonl,summary.json,summary.graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__cost_per_point",
                "description": "Join token usage with Linear estimates to compute tokens and dollars per story point, then validate and classify the cost curve through the durable cost-per-point LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/cost-per-point.{jsonl,summary.json,summary.graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__deploy_frequency",
                "description": "Aggregate deployment events into DORA cadence metrics, then validate and classify the deployment frequency verdict through the durable deploy-frequency LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/deploys-summary.{json,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__pr_review",
                "description": "Scan merged PR review health and human-in-the-loop coverage, then validate and classify the aggregate report through the durable PR-review LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/pr-review-summary.{json,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "window_days": {
                            "type": "integer",
                            "description": "Rolling merged-PR window in days. Defaults to 30."
                        }
                    }
                }
            },
            {
                "name": "sentinel__roi",
                "description": "Compute Claude-vs-human-team ROI from token and cost-per-point metrics, then validate and classify the headline through the durable ROI LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/roi.{jsonl,summary.json,summary.graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__sla",
                "description": "Aggregate SLA breach records, then validate and classify the operations verdict through the durable SLA LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/sla-breaches-summary.{json,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__token_cost",
                "description": "Price the SEN-7 token aggregate (tokens-per-ticket.jsonl) with Sentinel's configured Claude pricing table, then validate and classify the cached-vs-uncached cost report through the durable token cost LangGraph. Returns the summary plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/metrics/token-cost.{json,jsonl,graph-runs.jsonl}. Read-only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "sentinel__eval_run",
                "description": "Run the BA-Eval external benchmark from an explicit case_id -> candidate_output JSON artifact, persist the EvalRunResult, then authorize the aggregate verdict through the durable eval LangGraph. Returns the run plus checkpoint/write/stream/topology audit evidence. Writes ~/.claude/sentinel/eval/ba-corpus/runs/{run_id}.{json,graph-runs.jsonl}. Requires OPENROUTER_API_KEY.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "run_id": {
                            "type": "string",
                            "description": "Stable benchmark run id. Used for runs/{run_id}.json and the eval graph thread."
                        },
                        "candidates_path": {
                            "type": "string",
                            "description": "Path to a JSON object mapping case_id to candidate output text."
                        },
                        "case_ids": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional case_id allowlist. Empty or omitted runs all listed corpus cases."
                        },
                        "corpus_dir": {
                            "type": "string",
                            "description": "Optional BA-Eval corpus override. Defaults to ~/.claude/sentinel/eval/ba-corpus/."
                        },
                        "runs_dir": {
                            "type": "string",
                            "description": "Optional EvalRunResult output directory override. Defaults to ~/.claude/sentinel/eval/ba-corpus/runs/."
                        }
                    },
                    "required": ["run_id", "candidates_path"]
                }
            }
        ]
    })
}

/// Server info for MCP initialize response
fn server_info() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {"listChanged": true}
        },
        "serverInfo": {
            "name": "sentinel",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

pub async fn run() -> Result<()> {
    // The MCP server no longer pins a session id at startup. Each request
    // resolves its own session id via `resolve_session_id` (toolUseId
    // cross-reference → newest transcript mtime) and the `with_session_state`
    // transaction handles lock/load/save. The Arc<RwLock<SessionState>>
    // here is a request-scoped holder whose contents are overwritten on
    // every call.
    // See the header comment above `detect_live_session_id` for rationale.
    let state = Arc::new(RwLock::new(SessionState::new("uninitialized")));

    let judge: Arc<dyn sentinel_application::judge_service::JudgeService> = {
        let multi =
            sentinel_infrastructure::rig_judge::MultiModelJudge::from_env().map_err(|err| {
                anyhow::anyhow!("{err}; set OPENROUTER_API_KEY before starting sentinel mcp")
            })?;
        Arc::new(multi)
    };
    // Enterprise proof attestation is mandatory for the MCP authority surface.
    // No unsigned proof fallback: startup fails if either key is missing or bad.
    let signing_key = load_signing_key_from_env()?;
    let verify_key = load_verify_key_from_env()?;
    let proof_engine = Arc::new(
        ProofEngine::new(state.clone(), judge.clone())
            .with_signing(Some(signing_key), true)
            .with_verify_key(Some(verify_key))
            .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority))
            .with_step_graph_authority(Arc::new(CliPhaseGraphAuthority)),
    );
    let workflow_configs = load_workflow_configs()
        .map_err(|e| anyhow::anyhow!("workflow config load failed during MCP startup: {e}"))?;
    let step_configs = load_step_configs_for_workflows(&workflow_configs)
        .map_err(|e| anyhow::anyhow!("step config load failed during MCP startup: {e:#}"))?;
    // Wire cross-session proof archive backing (#39): query_proof_corpus
    // walks the index at ~/.claude/sentinel/proofs/index.jsonl in addition
    // to live state. MCP startup fails if home cannot be resolved because
    // live-only proof corpus responses are not allowed.
    let home = sentinel_infrastructure::paths::home_root_or_fatal();
    let fs: Arc<dyn sentinel_domain::ports::FileSystemPort> =
        Arc::new(sentinel_infrastructure::filesystem::RealFileSystem);
    let handler = McpHandler::new(state.clone(), proof_engine.clone())
        .with_workflows(workflow_configs.clone())
        .with_step_configs(step_configs.clone())
        .with_severity_graph_auditor(Arc::new(CliSeverityGraphAuditor))
        .with_pm_audit_graph_auditor(Arc::new(CliPmAuditGraphAuditor))
        .with_linear_health_graph_auditor(Arc::new(CliLinearHealthGraphAuditor))
        .with_dev_scorecard_graph_auditor(Arc::new(CliDevScorecardGraphAuditor))
        .with_token_cost_graph_auditor(Arc::new(CliTokenCostGraphAuditor))
        .with_token_usage_graph_auditor(Arc::new(CliTokenUsageGraphAuditor))
        .with_cache_efficiency_graph_auditor(Arc::new(CliCacheEfficiencyGraphAuditor))
        .with_cost_per_point_graph_auditor(Arc::new(CliCostPerPointGraphAuditor))
        .with_deploy_frequency_graph_auditor(Arc::new(CliDeployFrequencyGraphAuditor))
        .with_pr_review_graph_auditor(Arc::new(CliPrReviewGraphAuditor))
        .with_roi_graph_auditor(Arc::new(CliRoiGraphAuditor))
        .with_sla_graph_auditor(Arc::new(CliSlaGraphAuditor))
        .with_code_reconciliation_auditor(Arc::new(CliCodeReconciliationAuditor))
        .with_mcp_proof_read_graph_auditor(Arc::new(CliMcpProofReadGraphAuditor))
        .with_eval_runner(Arc::new(CliEvalRunGraphRunner))
        .with_ba_draft_runner(Arc::new(CliBaDraftGraphRunner))
        .with_archive(sentinel_application::mcp_handler::ProofArchiveBacking { home, fs });
    let handler = handler.with_llm(Arc::new(
        sentinel_infrastructure::openrouter_llm::OpenRouterLlm::from_env()
            .context("failed to build OpenRouter LLM for MCP enterprise LangGraph runtime")?,
    ));
    handler
        .validate_enterprise_langgraph_runtime()
        .context("MCP enterprise LangGraph runtime validation failed")?;

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    info!("Sentinel MCP server started (stdio)");

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            debug!("stdin closed, shutting down MCP server");
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to parse JSON-RPC request: {e}");
                continue;
            }
        };

        // Methods that don't read/write session state: dispatch directly.
        // Everything else: resolve session id, take the file lock, load
        // state into the shared Arc, run the handler, save and release.
        let needs_session = matches!(request.method.as_str(), "tools/call");

        let response = if needs_session {
            match resolve_session_id(&request.params) {
                Ok(session_id) => {
                    let handler_ref = &handler;
                    let state_ref = &state;
                    let proof_ref = &proof_engine;
                    let req_ref = &request;
                    let workflow_configs_ref = &workflow_configs;
                    match with_session_state(
                        &session_id,
                        state_ref,
                        workflow_configs_ref,
                        move || async move {
                            handle_request(req_ref, handler_ref, state_ref, proof_ref).await
                        },
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => JsonRpcResponse::error(
                            request.id.clone(),
                            -32000,
                            format!("Session state transaction failed: {e}"),
                        ),
                    }
                }
                Err(e) => JsonRpcResponse::error(
                    request.id.clone(),
                    -32000,
                    format!("Failed to resolve active Claude Code session: {e}"),
                ),
            }
        } else {
            handle_request(&request, &handler, &state, &proof_engine).await
        };

        let json = serde_json::to_string(&response)?;
        stdout.write_all(json.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await?;
    }

    Ok(())
}

async fn handle_request(
    request: &JsonRpcRequest,
    handler: &McpHandler,
    state: &Arc<RwLock<SessionState>>,
    proof_engine: &Arc<ProofEngine>,
) -> JsonRpcResponse {
    match request.method.as_str() {
        // MCP lifecycle
        "initialize" => JsonRpcResponse::success(request.id.clone(), server_info()),

        "initialized" | "notifications/initialized" => {
            // JSON-RPC notification — no response required by spec
            JsonRpcResponse::success(request.id.clone(), serde_json::json!({}))
        }

        // Tool listing
        "tools/list" => JsonRpcResponse::success(request.id.clone(), tool_definitions()),

        // Tool execution
        "tools/call" => {
            let tool_name = match request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                Some(tool_name) => tool_name,
                None => {
                    return JsonRpcResponse::error(
                        request.id.clone(),
                        -32602,
                        "tools/call requires non-empty string params.name",
                    );
                }
            };
            let arguments = match request.params.get("arguments") {
                Some(value) if value.is_object() => value.clone(),
                Some(_) => {
                    return JsonRpcResponse::error(
                        request.id.clone(),
                        -32602,
                        "tools/call params.arguments must be an object when present",
                    );
                }
                None => serde_json::json!({}),
            };

            // Handle submit_phase_complete specially (needs state mutation + proof generation)
            if tool_name == "sentinel__submit_phase_complete" {
                return handle_submit_phase(request, &arguments, state, proof_engine).await;
            }
            if tool_name == "sentinel__submit_step_complete" {
                return handle_submit_step_complete(request, &arguments, state, handler).await;
            }
            if tool_name == "sentinel__record_dyad_verdict" {
                return handle_record_dyad_verdict(request, &arguments, state).await;
            }

            // Handle step tracking tools specially (need state mutation)
            if tool_name == "sentinel__update_step" {
                return handle_update_step(request, &arguments, state).await;
            }
            if tool_name == "sentinel__get_workflow_status" {
                return handle_get_workflow_status(request, &arguments, state).await;
            }
            if tool_name == "sentinel__get_phase_steps" {
                return handle_get_phase_steps(request, &arguments, state).await;
            }
            if tool_name == "sentinel__get_workflow_progress" {
                return handle_get_workflow_progress(request, &arguments, state).await;
            }
            if tool_name == "sentinel__replay_phase" {
                return handle_replay_phase(request, &arguments, state).await;
            }

            // CLAUDE.md management — shared implementation with the CLI
            // subcommands lives in `crate::claude_md_cmd`.
            if tool_name == "sentinel__regenerate_claude_md" {
                return handle_operational_tool_result(
                    request,
                    "regenerate_claude_md",
                    serde_json::json!({}),
                    crate::claude_md_cmd::regenerate(),
                )
                .await;
            }
            if tool_name == "sentinel__edit_claude_md_template" {
                let find = match arguments
                    .get("find")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    Some(value) => value,
                    None => {
                        return JsonRpcResponse::error(
                            request.id.clone(),
                            -32602,
                            "sentinel__edit_claude_md_template requires non-empty string argument `find`",
                        );
                    }
                };
                let replace = match arguments.get("replace").and_then(|v| v.as_str()) {
                    Some(value) => value,
                    None => {
                        return JsonRpcResponse::error(
                            request.id.clone(),
                            -32602,
                            "sentinel__edit_claude_md_template requires string argument `replace`",
                        );
                    }
                };
                return handle_operational_tool_result(
                    request,
                    "edit_claude_md_template",
                    serde_json::json!({
                        "find_sha256": sentinel_infrastructure::operational_tool_graph::sha256_json(
                            &serde_json::json!(find)
                        ),
                        "replace_sha256": sentinel_infrastructure::operational_tool_graph::sha256_json(
                            &serde_json::json!(replace)
                        ),
                    }),
                    crate::claude_md_cmd::edit_template(find, replace),
                )
                .await;
            }
            if tool_name == "sentinel__restart_all_mcps" {
                return handle_operational_tool_result(
                    request,
                    "restart_all_mcps",
                    serde_json::json!({}),
                    crate::claude_md_cmd::restart_all_mcps(),
                )
                .await;
            }

            // SEN-8: WIP-by-stage snapshot read. The poller is a separate
            // task; this only surfaces whatever the file contains right now.
            if tool_name == "sentinel__route_capability" {
                return handle_route_capability(request, &arguments).await;
            }

            // Worker delegation (#2): hand a unit of work to a worker model.
            if tool_name == "sentinel__delegate_codex" {
                return handle_delegate(
                    request,
                    &arguments,
                    sentinel_application::delegation_service::Worker::Codex,
                )
                .await;
            }
            if tool_name == "sentinel__delegate_kimi_context_scan" {
                return handle_delegate(
                    request,
                    &arguments,
                    sentinel_application::delegation_service::Worker::Kimi,
                )
                .await;
            }

            if tool_name == "sentinel__get_wip_snapshot" {
                let mut response = match sentinel_application::wip_snapshot::read() {
                    Ok(Some(snap)) => serde_json::json!({
                        "captured_at": snap.captured_at,
                        "total_wip": snap.total_wip(),
                        "teams": snap.teams,
                        "bottlenecks": snap.bottlenecks,
                    }),
                    Ok(None) => serde_json::json!({
                        "captured_at": null,
                        "message": "no snapshot captured yet - poller has not run"
                    }),
                    Err(e) => {
                        return JsonRpcResponse::success(
                            request.id.clone(),
                            mcp_tool_result(false, serde_json::json!({"error": e.to_string()})),
                        );
                    }
                };
                let graph_audit = match run_wip_snapshot_graph_audit(&response).await {
                    Ok(audit) => audit,
                    Err(e) => {
                        return JsonRpcResponse::success(
                            request.id.clone(),
                            mcp_tool_result(
                                false,
                                langgraph_tool_error(format!(
                                    "wip snapshot graph authority failed: {e:#}"
                                )),
                            ),
                        );
                    }
                };
                if let Some(object) = response.as_object_mut() {
                    object.insert(
                        "workflow_authority".to_string(),
                        serde_json::json!("langgraph"),
                    );
                    object.insert("graph_audit".to_string(), graph_audit);
                }
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(true, response),
                );
            }

            // Handle get_session_stats specially
            if tool_name == "sentinel__get_session_stats" {
                let workflow_configs = match load_workflow_configs_for_rpc(request) {
                    Ok(configs) => configs,
                    Err(response) => return response,
                };
                let (
                    session_id,
                    active_skill,
                    total_invocations,
                    total_blocked,
                    per_hook,
                    mut proof_chains,
                ) = {
                    let s = state.read().await;
                    (
                        s.session_id.clone(),
                        s.active_skill.clone(),
                        s.hook_stats.total_invocations,
                        s.hook_stats.total_blocked,
                        s.hook_stats.per_hook.clone(),
                        s.proof_chain_skills().cloned().collect::<Vec<_>>(),
                    )
                };
                proof_chains.sort();
                let projected =
                    match graph_projected_workflows(&session_id, &workflow_configs).await {
                        Ok(projected) => projected,
                        Err(e) => {
                            return JsonRpcResponse::success(
                                request.id.clone(),
                                mcp_tool_result(
                                    false,
                                    langgraph_tool_error(format!(
                                        "phase graph checkpoint projection failed: {e}"
                                    )),
                                ),
                            );
                        }
                    };
                let mut langgraph_workflows = projected.keys().cloned().collect::<Vec<_>>();
                langgraph_workflows.sort();
                let mut stats = serde_json::json!({
                    "session_id": session_id,
                    "active_skill": active_skill,
                    "total_invocations": total_invocations,
                    "total_blocked": total_blocked,
                    "per_hook": per_hook,
                    "langgraph_workflows": langgraph_workflows,
                    "langgraph_workflow_count": projected.len(),
                    "proof_chains": proof_chains,
                });
                let graph_audit = match run_session_stats_graph_audit(&stats).await {
                    Ok(audit) => audit,
                    Err(e) => {
                        return JsonRpcResponse::success(
                            request.id.clone(),
                            mcp_tool_result(
                                false,
                                langgraph_tool_error(format!(
                                    "session stats graph authority failed: {e:#}"
                                )),
                            ),
                        );
                    }
                };
                if let Some(object) = stats.as_object_mut() {
                    object.insert(
                        "workflow_authority".to_string(),
                        serde_json::json!("langgraph"),
                    );
                    object.insert("graph_audit".to_string(), graph_audit);
                }
                return JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, stats));
            }

            let call = McpToolCall {
                name: tool_name.to_string(),
                arguments,
            };

            let result = handler.handle(call).await;

            if result.success {
                JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result.content))
            } else {
                JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(result.error.unwrap_or_else(|| {
                            format!("{} failed without error detail", tool_name)
                        })),
                    ),
                )
            }
        }

        // Ping
        "ping" => JsonRpcResponse::success(request.id.clone(), serde_json::json!({})),

        // Unknown method
        method => JsonRpcResponse::error(
            request.id.clone(),
            -32601,
            format!("Method not found: {method}"),
        ),
    }
}

async fn handle_operational_tool_result(
    request: &JsonRpcRequest,
    operation: &str,
    input: serde_json::Value,
    result: Result<serde_json::Value>,
) -> JsonRpcResponse {
    let result = match result {
        Ok(value) => value,
        Err(e) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error(e.to_string())),
            );
        }
    };
    let graph_audit =
        match crate::claude_md_cmd::run_operational_tool_graph_audit(operation, &input, &result)
            .await
        {
            Ok(audit) => audit,
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "operational tool graph authority failed: {e:#}"
                        )),
                    ),
                );
            }
        };
    JsonRpcResponse::success(
        request.id.clone(),
        mcp_tool_result(
            true,
            crate::claude_md_cmd::attach_operational_tool_graph_audit(result, graph_audit),
        ),
    )
}

async fn run_wip_snapshot_graph_audit(response: &serde_json::Value) -> Result<serde_json::Value> {
    let response_hash = sentinel_infrastructure::wip_snapshot_graph::sha256_json(response);
    let snapshot_present = response
        .get("captured_at")
        .is_some_and(serde_json::Value::is_string);
    let identifier = format!("present-{snapshot_present}:response-{response_hash}");
    let state = sentinel_infrastructure::wip_snapshot_graph::WipSnapshotState::from_response(
        identifier, response,
    );
    let graph = sentinel_infrastructure::wip_snapshot_graph::build_wip_snapshot_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build wip snapshot graph: {e}"))?;
    let run = sentinel_infrastructure::wip_snapshot_graph::run_wip_snapshot_decision_report(
        &graph, state,
    )
    .await
    .map_err(|e| anyhow::anyhow!("run wip snapshot graph: {e}"))?;
    let authorization = run
        .wip_snapshot_authorization()
        .map_err(|e| anyhow::anyhow!("wip snapshot graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("wip snapshot graph produced no terminal checkpoint"))?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("wip-snapshot.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create wip snapshot graph audit dir {}", parent.display()))?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "wip_snapshot",
        "decision": sentinel_infrastructure::wip_snapshot_graph::
            wip_snapshot_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .with_context(|| format!("open wip snapshot graph audit {}", graph_runs.display()))?;
    serde_json::to_writer(&mut file, &row)
        .with_context(|| format!("write wip snapshot graph audit {}", graph_runs.display()))?;
    file.write_all(b"\n").with_context(|| {
        format!(
            "terminate wip snapshot graph audit {}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "wip_snapshot",
        "graph_runs_path": graph_runs,
        "decision": sentinel_infrastructure::wip_snapshot_graph::
            wip_snapshot_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

async fn run_session_stats_graph_audit(stats: &serde_json::Value) -> Result<serde_json::Value> {
    let stats_hash = sentinel_infrastructure::session_stats_graph::sha256_json(stats);
    let session_id = stats
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("session stats LangGraph audit requires session_id"))?;
    let identifier = format!("{session_id}:stats-{stats_hash}");
    let state = sentinel_infrastructure::session_stats_graph::SessionStatsState::from_response(
        identifier, stats,
    );
    let graph = sentinel_infrastructure::session_stats_graph::build_session_stats_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build session stats graph: {e}"))?;
    let run = sentinel_infrastructure::session_stats_graph::run_session_stats_decision_report(
        &graph, state,
    )
    .await
    .map_err(|e| anyhow::anyhow!("run session stats graph: {e}"))?;
    let authorization = run
        .session_stats_authorization()
        .map_err(|e| anyhow::anyhow!("session stats graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("session stats graph produced no terminal checkpoint"))?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("session-stats.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create session stats graph audit dir {}", parent.display())
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "session_stats",
        "decision": sentinel_infrastructure::session_stats_graph::
            session_stats_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .with_context(|| format!("open session stats graph audit {}", graph_runs.display()))?;
    serde_json::to_writer(&mut file, &row)
        .with_context(|| format!("write session stats graph audit {}", graph_runs.display()))?;
    file.write_all(b"\n").with_context(|| {
        format!(
            "terminate session stats graph audit {}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "session_stats",
        "graph_runs_path": graph_runs,
        "decision": sentinel_infrastructure::session_stats_graph::
            session_stats_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

/// Handle `sentinel__route_capability` — A2 capability router exposure.
///
/// Loads the shipped agent profiles + optional operator overrides
/// (`~/.claude/sentinel/config/agents.toml`), runs the requirement
/// through the pure routing algorithm, and returns the full
/// [`sentinel_domain::agent_routing::RoutingExplanation`].
///
/// Synchronous: no async I/O on the hot path (TOML load + in-memory
/// pick). The MCP dispatcher's overall arm is `async fn`; this helper
/// borrows that context but doesn't await.
async fn handle_route_capability(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
) -> JsonRpcResponse {
    use sentinel_domain::capability::CapabilityRequirement;
    use sentinel_domain::ports::CapabilityRouterPort;
    use sentinel_infrastructure::capability_router::TomlCapabilityRouter;

    let requirement: CapabilityRequirement = match args.get("requirement") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(r) => r,
            Err(err) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        serde_json::json!({
                            "error": format!(
                                "could not parse `requirement` field as CapabilityRequirement: {err}. \
                                 Shape: {{\"required\": [<Capability>...], \"preferred\": [], \"forbidden\": []}}. \
                                 See docs/a2-capability-aware-routing.md §2."
                            )
                        }),
                    ),
                );
            }
        },
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    serde_json::json!({
                        "error": "missing required argument: `requirement` (CapabilityRequirement JSON)"
                    }),
                ),
            );
        }
    };

    let overrides_path = sentinel_infrastructure::config::config_dir().join("agents.toml");
    let router = match TomlCapabilityRouter::with_shipped_and_overrides(Some(&overrides_path)) {
        Ok(r) => r,
        Err(err) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    serde_json::json!({
                        "error": format!("failed to load agent profiles: {err}")
                    }),
                ),
            );
        }
    };

    let explanation = router.explain(&requirement);
    let graph_audit =
        match run_capability_route_graph_audit(&requirement, &explanation, router.profiles().len())
            .await
        {
            Ok(audit) => audit,
            Err(err) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "capability routing graph authority failed: {err:#}"
                        )),
                    ),
                );
            }
        };

    let mut response = match serde_json::to_value(&explanation) {
        Ok(value) => value,
        Err(err) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!("capability routing serialization failed: {err}")),
                ),
            );
        }
    };
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
        obj.insert("graph_audit".to_string(), graph_audit);
    }

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, response))
}

async fn run_capability_route_graph_audit(
    requirement: &sentinel_domain::capability::CapabilityRequirement,
    explanation: &sentinel_domain::agent_routing::RoutingExplanation,
    profile_count: usize,
) -> Result<serde_json::Value> {
    let identifier = format!(
        "{}:profiles-{}:chosen-{}:candidates-{}",
        explanation.requirement_signature,
        profile_count,
        explanation.chosen.is_some(),
        explanation.candidates.len()
    );
    let state =
        sentinel_infrastructure::capability_route_graph::CapabilityRouteState::from_explanation(
            identifier,
            requirement,
            explanation,
            profile_count,
        );
    let graph = sentinel_infrastructure::capability_route_graph::build_capability_route_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build capability route graph: {e}"))?;
    let run =
        sentinel_infrastructure::capability_route_graph::run_capability_route_decision_report(
            &graph, state,
        )
        .await
        .map_err(|e| anyhow::anyhow!("run capability route graph: {e}"))?;
    let authorization = run
        .capability_route_authorization()
        .map_err(|e| anyhow::anyhow!("capability route graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("capability route graph produced no terminal checkpoint"))?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("capability-route.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "create capability route graph audit dir {}",
                parent.display()
            )
        })?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "capability_route",
        "decision": sentinel_infrastructure::capability_route_graph::
            capability_route_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .with_context(|| format!("open capability route graph audit {}", graph_runs.display()))?;
    serde_json::to_writer(&mut file, &row).with_context(|| {
        format!(
            "write capability route graph audit {}",
            graph_runs.display()
        )
    })?;
    file.write_all(b"\n").with_context(|| {
        format!(
            "terminate capability route graph audit {}",
            graph_runs.display()
        )
    })?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "capability_route",
        "graph_runs_path": graph_runs,
        "decision": sentinel_infrastructure::capability_route_graph::
            capability_route_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

async fn run_delegation_graph_audit(
    request: &sentinel_application::delegation_service::DelegationRequest,
    result: &sentinel_application::delegation_service::DelegationResult,
) -> Result<serde_json::Value> {
    let identifier = format!(
        "{}:task-{}:context-{}:max-{}:output-{}",
        request.worker.label(),
        sentinel_infrastructure::delegation_graph::sha256(&request.task),
        sentinel_infrastructure::delegation_graph::sha256(&request.context),
        request.max_tokens,
        sentinel_infrastructure::delegation_graph::sha256(&result.output)
    );
    let state = sentinel_infrastructure::delegation_graph::DelegationState::from_result(
        identifier, request, result,
    );
    let graph = sentinel_infrastructure::delegation_graph::build_delegation_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build delegation graph: {e}"))?;
    let run =
        sentinel_infrastructure::delegation_graph::run_delegation_decision_report(&graph, state)
            .await
            .map_err(|e| anyhow::anyhow!("run delegation graph: {e}"))?;
    let authorization = run
        .delegation_authorization()
        .map_err(|e| anyhow::anyhow!("delegation graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("delegation graph produced no terminal checkpoint"))?;
    let graph_runs = sentinel_infrastructure::paths::sentinel_root()
        .join("metrics")
        .join("delegation.graph-runs.jsonl");
    if let Some(parent) = graph_runs.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create delegation graph audit dir {}", parent.display()))?;
    }
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "delegation",
        "decision": sentinel_infrastructure::delegation_graph::
            delegation_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": run.thread_id.clone(),
        "run": run,
    });
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&graph_runs)
        .with_context(|| format!("open delegation graph audit {}", graph_runs.display()))?;
    serde_json::to_writer(&mut file, &row)
        .with_context(|| format!("write delegation graph audit {}", graph_runs.display()))?;
    file.write_all(b"\n")
        .with_context(|| format!("terminate delegation graph audit {}", graph_runs.display()))?;
    Ok(serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "delegation",
        "graph_runs_path": graph_runs,
        "decision": sentinel_infrastructure::delegation_graph::
            delegation_decision_label(authorization.decision()),
        "authorization_checkpoint": authorization.checkpoint_ref(),
        "thread_id": row["thread_id"].clone(),
        "run": row["run"].clone(),
    }))
}

/// Handle `sentinel__delegate_codex` / `sentinel__delegate_kimi_context_scan`
/// (#2). Hands a unit of work to a worker model via the standardized
/// `OpenRouterLlm` path and returns the structured result.
///
/// Codex reads `task` + optional `context`; Kimi reads `question` + `content`
/// (mapped onto the same `task`/`context` slots of the delegation request).
async fn handle_delegate(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    worker: sentinel_application::delegation_service::Worker,
) -> JsonRpcResponse {
    use sentinel_application::delegation_service::{
        delegate, DelegationRequest, Worker, DEFAULT_MAX_TOKENS,
    };

    // Codex uses task/context; Kimi uses question/content. Normalize both onto
    // the request's task/context fields.
    let (task_key, ctx_key) = match worker {
        Worker::Codex => ("task", "context"),
        Worker::Kimi => ("question", "content"),
    };
    let Some(task) = args
        .get(task_key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                serde_json::json!({"error": format!("missing required non-empty string argument: `{task_key}`")}),
            ),
        );
    };
    let context = match (worker, args.get(ctx_key)) {
        (Worker::Codex, None) => "",
        (Worker::Codex, Some(value)) => match value.as_str() {
            Some(context) => context,
            None => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        serde_json::json!({"error": "`context` must be a string when present"}),
                    ),
                );
            }
        },
        (Worker::Kimi, Some(value)) => match value.as_str().map(str::trim) {
            Some(content) if !content.is_empty() => content,
            _ => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        serde_json::json!({"error": "missing required non-empty string argument: `content`"}),
                    ),
                );
            }
        },
        (Worker::Kimi, None) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    serde_json::json!({"error": "missing required non-empty string argument: `content`"}),
                ),
            );
        }
    };
    let max_tokens = match args.get("max_tokens") {
        None => DEFAULT_MAX_TOKENS,
        Some(value) => match value.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) if n > 0 => n,
            _ => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        serde_json::json!({"error": "`max_tokens` must be a positive integer when present"}),
                    ),
                );
            }
        },
    };

    let llm = match sentinel_infrastructure::openrouter_llm::OpenRouterLlm::from_env() {
        Ok(l) => l,
        Err(e) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    serde_json::json!({
                        "error": format!("worker delegation unavailable: {e} (set OPENROUTER_API_KEY)")
                    }),
                ),
            );
        }
    };

    let req = DelegationRequest {
        worker,
        task: task.to_string(),
        context: context.to_string(),
        max_tokens,
    };
    match delegate(&llm, &req).await {
        Ok(res) => {
            let graph_audit = match run_delegation_graph_audit(&req, &res).await {
                Ok(audit) => audit,
                Err(e) => {
                    return JsonRpcResponse::success(
                        request.id.clone(),
                        mcp_tool_result(
                            false,
                            langgraph_tool_error(format!(
                                "worker delegation graph authority failed: {e:#}"
                            )),
                        ),
                    );
                }
            };
            JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    true,
                    serde_json::json!({
                        "workflow_authority": "langgraph",
                        "worker": res.worker,
                        "output": res.output,
                        "graph_audit": graph_audit,
                    }),
                ),
            )
        }
        Err(e) => JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                serde_json::json!({"error": format!("worker delegation failed: {e}")}),
            ),
        ),
    }
}

/// Wrapper for the graph-aware application submit-step handler.
async fn handle_submit_step_complete(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    _state: &Arc<RwLock<SessionState>>,
    handler: &McpHandler,
) -> JsonRpcResponse {
    let result = handler
        .handle(McpToolCall {
            name: "sentinel__submit_step_complete".to_string(),
            arguments: args.clone(),
        })
        .await;
    if result.success {
        let mut content = result.content;
        attach_phase_graph_response_authority(&mut content);
        JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, content))
    } else {
        JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(result.error.unwrap_or_else(|| {
                    "sentinel__submit_step_complete failed without error detail".to_string()
                })),
            ),
        )
    }
}

/// Handle `sentinel__submit_phase_complete`
async fn handle_submit_phase(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
    proof_engine: &Arc<ProofEngine>,
) -> JsonRpcResponse {
    let skill = match args.get("skill").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
            );
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'phase_id'")),
            );
        }
    };
    let summary = match args.get("summary").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'summary'")),
            );
        }
    };
    let started_at = match args.get("started_at").and_then(|v| v.as_str()) {
        Some(ts) => match chrono::DateTime::parse_from_rfc3339(ts) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "Invalid 'started_at' (expected RFC3339): {e}"
                        )),
                    ),
                );
            }
        },
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'started_at'")),
            );
        }
    };

    // Look up phase config for judge model + objectives from workflows.toml
    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };

    // **Attack #142 fix**: Verify the skill has a workflow definition before
    // recording phase reads. Without this check, an attacker could submit evidence
    // for a non-existent skill, creating workflow state entries that have no
    // enforcement gates (no phases to complete, so everything passes).
    let workflow_config = match workflow_configs.get(&skill) {
        Some(workflow_config) => workflow_config,
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!(
                        "No workflow definition for skill '{}'. Cannot submit evidence.",
                        skill
                    )),
                ),
            );
        }
    };
    let phase_config = match workflow_config
        .phases
        .iter()
        .find(|phase| phase.id == phase_id)
    {
        Some(phase) => phase,
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!(
                        "Unknown phase '{}' for workflow '{}'. Cannot submit evidence.",
                        phase_id, skill
                    )),
                ),
            );
        }
    };

    let judge_model = phase_config.judge;
    let phase_objectives = if phase_config.description.is_empty() {
        format!("Complete the {phase_id} phase")
    } else {
        phase_config.description.clone()
    };

    let session_id_for_graph = { state.read().await.session_id.clone() };
    let graph_workflow_state =
        match graph_latest_workflow_state(&skill, &session_id_for_graph, &workflow_configs).await {
            Ok(state) => state,
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "phase graph checkpoint load failed before proof submission: {e}"
                        )),
                    ),
                );
            }
        };
    let Some(graph_workflow_state) = graph_workflow_state else {
        {
            let mut s = state.write().await;
            s.remove_graph_projected_workflow(&skill);
            if let Err(e) = sentinel_infrastructure::state_store::save(&mut s) {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "failed to persist stale workflow cleanup before proof submission: {e}"
                        )),
                    ),
                );
            }
        }
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!(
                    "Phase '{phase_id}' for skill '{skill}' requires an existing LangGraph checkpoint before sentinel__submit_phase_complete. Run the phase gate first so the verdict answers a durable graph interrupt."
                )),
            ),
        );
    };

    // Record phase-read evidence only after graph preflight succeeds. The
    // workflow projection is seeded from the graph checkpoint so the proof
    // engine never applies a verdict against session-local progress.
    let phase_file = format!("{phase_id}.md");
    {
        let mut s = state.write().await;
        s.set_active_skill_marker(&skill);
        s.set_graph_projected_workflow(skill.clone(), graph_workflow_state.clone());
        s.record_phase_read(&skill, &phase_file);
    }

    // Build evidence from the summary + state context
    let evidence = {
        let s = state.read().await;
        let mut ev = Evidence::default();
        let completed_phases = Some(graph_workflow_state.completed_phases.clone());
        ev.phase_file_read = true;
        ev.custom = serde_json::json!({
            "summary": summary,
            "phases_read": s.phases_read,
            "tool_calls_in_session": s.tool_calls,
            "hook_invocations": s.hook_stats.total_invocations,
            "blocked_count": s.hook_stats.total_blocked,
            "completed_phases": completed_phases,
            "active_skill": s.active_skill,
        });

        // Include step evidence from the graph checkpoint, not the session cache.
        let step_states = graph_workflow_state.phase_step_states(&phase_id);
        for ss in &step_states {
            match ss.status {
                StepStatus::Completed => ev.steps_completed.push(ss.step_id.clone()),
                StepStatus::Skipped => ev.steps_skipped.push(ss.step_id.clone()),
                _ => {}
            }
        }
        ev
    };

    // High-stakes opt-in: when the submission sets `dual: true`, the
    // completion verdict runs the cross-vendor DualFrontier tier (Opus 4.8 +
    // GPT-5.5) instead of the single configured judge_model. A wrong "done" is
    // sentinel's most expensive error, so callers can demand two adversarial
    // frontier opinions for the phases that warrant it.
    let dual = args
        .get("dual")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Generate cryptographic proof via the proof engine. Phase timing is
    // caller-supplied authority data; do not approximate it here.
    let proof_result = proof_engine
        .submit_evidence_report(
            &skill,
            &phase_id,
            &phase_objectives,
            evidence,
            judge_model,
            started_at,
            Some(workflow_config),
            dual,
        )
        .await;

    {
        let mut s = state.write().await;
        if let Err(e) = sentinel_infrastructure::state_store::save(&mut s) {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!(
                        "failed to persist session state after proof submission: {e}"
                    )),
                ),
            );
        }
    }

    match proof_result {
        Ok(report) => {
            let completed = report.phase_graph.workflow_state.completed_phases.clone();
            let tessera = report.proof.combined_hash[..12].to_string();
            let phase_graph = report.phase_graph.graph_run;
            // SUCCESS — minimal info, no judge reasoning exposed
            let mut result = serde_json::json!({
                "phase_id": phase_id,
                "status": "accepted",
                "tessera": tessera,
                "completed_phases": completed,
                "phase_graph": phase_graph,
            });
            attach_phase_graph_response_authority(&mut result);
            JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
        }
        Err(e) => {
            // BLOCKED — judge reasoning stays opaque, but graph authority
            // failures are operational/security errors and should be surfaced.
            warn!(phase = %phase_id, error = %e, "Phase submission blocked");
            let full_error = format!("{e:#}");
            let error_msg = if full_error.contains("LangGraph")
                || full_error.contains("graph")
                || full_error.contains("cannot advance")
                || full_error.contains("cannot be advanced")
                || full_error.contains("already completed")
                || full_error.contains("replay the phase")
            {
                format!("Phase '{phase_id}' rejected by phase graph: {full_error}")
            } else {
                format!(
                    "Phase '{phase_id}' BLOCKED — evidence insufficient. Re-run the phase with complete outputs before re-submitting."
                )
            };
            JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error(error_msg)),
            )
        }
    }
}

/// Handle `sentinel__record_dyad_verdict`.
async fn handle_record_dyad_verdict(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let Some(skill) = args.get("skill").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
        );
    };
    let Some(phase_id) = args.get("phase_id").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'phase_id'")),
        );
    };
    let Some(role) = args.get("role").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'role'")),
        );
    };
    let Some(agent) = args.get("agent").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'agent'")),
        );
    };
    let agent = agent.trim();
    if agent.is_empty() {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("agent must be non-empty")),
        );
    }

    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };
    let Some(workflow) = workflow_configs.get(skill) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("No workflow for '{skill}'")),
            ),
        );
    };
    let Some(phase) = workflow.phases.iter().find(|phase| phase.id == phase_id) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("Unknown phase '{phase_id}' for workflow '{skill}'")),
            ),
        );
    };
    let Some(required_dyad) = phase.required_dyad else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!(
                    "Phase '{phase_id}' for workflow '{skill}' does not declare required_dyad"
                )),
            ),
        );
    };

    let mut verdicts = DyadVerdicts::default();
    match role {
        "implementer" => {
            verdicts.implementer = Some(agent.to_string());
        }
        "reviewer" => {
            if !required_dyad.reviewer {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "Phase '{phase_id}' for workflow '{skill}' does not require a reviewer dyad"
                        )),
                    ),
                );
            }
            verdicts.reviewer_pass_by = Some(agent.to_string());
        }
        "tester" => {
            if !required_dyad.tester {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "Phase '{phase_id}' for workflow '{skill}' does not require a tester dyad"
                        )),
                    ),
                );
            }
            verdicts.tester_pass_by = Some(agent.to_string());
        }
        other => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!(
                        "Invalid role '{other}'. Use: implementer, reviewer, tester"
                    )),
                ),
            );
        }
    }

    let session_id = { state.read().await.session_id.clone() };
    let outcome = async {
        let db_path = phase_graph_db_path(&session_id)?;
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph_state = graph
            .update_dyad_verdicts(skill, &session_id, phase_id, verdicts)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let phase_graph = phase_graph_mutation_evidence(&graph, &session_id, &graph_state).await?;
        Ok::<_, anyhow::Error>((graph_state, phase_graph))
    }
    .await;

    match outcome {
        Ok((graph_state, phase_graph)) => {
            let dyad_verdict = graph_state.dyad_verdicts.get(phase_id).cloned();
            let persist_result = {
                let mut s = state.write().await;
                s.set_active_skill_marker(skill);
                s.set_graph_projected_workflow(skill.to_string(), graph_state.to_workflow_state());
                sentinel_infrastructure::state_store::save(&mut s)
            };
            if let Err(e) = persist_result {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!("failed to persist dyad graph state: {e}")),
                    ),
                );
            }
            JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(true, {
                    let mut result = serde_json::json!({
                    "status": "accepted",
                    "skill": skill,
                    "phase_id": phase_id,
                    "role": role,
                    "agent": agent,
                    "required_dyad": required_dyad,
                    "dyad_verdict": dyad_verdict,
                    "phase_graph": phase_graph,
                    });
                    attach_phase_graph_response_authority(&mut result);
                    result
                }),
            )
        }
        Err(e) => JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("dyad verdict graph update failed: {e}")),
            ),
        ),
    }
}

/// Helper: load the required step plan for a configured LangGraph workflow.
fn load_required_steps_config_for_rpc(
    request: &JsonRpcRequest,
    skill: &str,
) -> std::result::Result<SkillSteps, JsonRpcResponse> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    match sentinel_infrastructure::config::load_skill_steps(&config_dir, skill) {
        Ok(Some(steps)) => Ok(steps),
        Ok(None) => Err(JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!(
                    "configured LangGraph workflow '{skill}' is missing required step config '{}'",
                    config_dir
                        .join("steps")
                        .join(format!("{skill}.toml"))
                        .display()
                )),
            ),
        )),
        Err(e) => Err(JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!(
                    "failed to load step config for configured LangGraph workflow '{skill}': {e:#}"
                )),
            ),
        )),
    }
}

/// Handle `sentinel__update_step`
async fn handle_update_step(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let skill = match args.get("skill").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
            );
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'phase_id'")),
            );
        }
    };
    let step_id = match args.get("step_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'step_id'")),
            );
        }
    };
    let status_str = match args.get("status").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'status'")),
            );
        }
    };
    let summary = args
        .get("summary")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);

    // Parse status explicitly so the MCP contract stays stable (`in_progress`)
    // instead of inheriting serde's enum rename spelling.
    let status = match status_str.as_str() {
        "pending" => StepStatus::Pending,
        "blocked" => StepStatus::Blocked,
        "in_progress" => StepStatus::InProgress,
        "completed" => StepStatus::Completed,
        "skipped" => StepStatus::Skipped,
        _ => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!(
                        "Invalid status '{}'. Use: pending, blocked, in_progress",
                        status_str
                    )),
                ),
            );
        }
    };
    if matches!(status, StepStatus::Completed | StepStatus::Skipped) {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!(
                    "Terminal step status '{status_str}' cannot be written through sentinel__update_step. Use sentinel__submit_step_complete so StepProof sealing and LangGraph checkpoint evidence are committed together."
                )),
            ),
        );
    }

    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };
    let Some(workflow) = workflow_configs.get(&skill) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("No workflow for '{skill}'")),
            ),
        );
    };
    let steps_config = match load_required_steps_config_for_rpc(request, &skill) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let Some(step_policy) = steps_config
        .phase_steps(&phase_id)
        .and_then(|phase_steps| phase_steps.steps.iter().find(|step| step.id == step_id))
        .cloned()
    else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!(
                    "configured step policy for '{skill}/{phase_id}.{step_id}' is missing"
                )),
            ),
        );
    };

    let outcome = async {
        let session_id = {
            let s = state.read().await;
            s.session_id.clone()
        };

        let db_path = phase_graph_db_path(&session_id)?;
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph_state = graph
            .update_step(
                &skill,
                &session_id,
                &phase_id,
                &step_id,
                &step_policy,
                status.clone(),
                summary.clone(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let phase_graph = phase_graph_mutation_evidence(&graph, &session_id, &graph_state).await?;
        let workflow_state = graph_state.to_workflow_state();
        {
            let mut s = state.write().await;
            s.set_active_skill_marker(&skill);
            s.set_graph_projected_workflow(skill.clone(), workflow_state.clone());
            sentinel_infrastructure::state_store::save(&mut s).map_err(|e| {
                anyhow::anyhow!("failed to persist state after graph step update: {e}")
            })?;
        }

        Ok::<_, anyhow::Error>((workflow_state, phase_graph))
    }
    .await;

    let (workflow_state, phase_graph) = match outcome {
        Ok(result) => result,
        Err(e) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!("step graph update failed: {e}")),
                ),
            );
        }
    };

    // Compute progress from the graph-projected workflow state.
    let phase_completed = workflow_state.phase_steps_completed(&phase_id);

    // Phase total comes from the authoritative step plan. Runtime state is not
    // a substitute for a missing plan.
    let phase_total = steps_config
        .phase_steps(&phase_id)
        .map_or(0, |ps| ps.steps.len());

    let phase_progress = format!("{phase_completed}/{phase_total} steps");

    let overall_progress = {
        let total = steps_config.total_steps();
        let completed = workflow_state.total_steps_completed();
        format!("{completed}/{total} steps")
    };

    let mut result = serde_json::json!({
        "step_id": step_id,
        "phase_id": phase_id,
        "status": status_str,
        "phase_progress": phase_progress,
        "phase_graph": phase_graph,
    });
    attach_phase_graph_response_authority(&mut result);

    result.as_object_mut().unwrap().insert(
        "overall_progress".to_string(),
        serde_json::json!(overall_progress),
    );

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Handle `sentinel__get_workflow_status`
async fn handle_get_workflow_status(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let skill = match args.get("skill").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
            );
        }
    };
    let session_id = { state.read().await.session_id.clone() };
    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };
    if !workflow_configs.contains_key(&skill) {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("No configured LangGraph workflow for '{skill}'")),
            ),
        );
    }

    let graph_state =
        match graph_latest_workflow_state(&skill, &session_id, &workflow_configs).await {
            Ok(state) => state,
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!("phase graph checkpoint load failed: {e}")),
                    ),
                );
            }
        };
    let graph_topology = match graph_introspection(&skill, &session_id, &workflow_configs).await {
        Ok(topology) => topology,
        Err(e) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!("phase graph introspection failed: {e}")),
                ),
            );
        }
    };

    let mut result = if let Some(wf) = graph_state {
        match serde_json::to_value(wf) {
            Ok(value) => value,
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!("workflow state serialization failed: {e}")),
                    ),
                );
            }
        }
    } else {
        serde_json::json!({
            "skill": skill.clone(),
            "workflow_authority": "langgraph",
            "status": "no_checkpoint",
            "checkpoint": null,
            "graph_state": null,
        })
    };
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "workflow_authority".to_string(),
            serde_json::json!("langgraph"),
        );
        if let Some(topology) = graph_topology {
            obj.insert("graph_topology".to_string(), serde_json::json!(topology));
        }
    }
    if let Some(obj) = result.as_object_mut() {
        if let Err(error) =
            attach_phase_graph_read_evidence(obj, &skill, &session_id, &workflow_configs).await
        {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error(error)),
            );
        }
    }
    let result =
        match attach_workflow_read_graph_audit(WorkflowApiReadSurface::Status, result).await {
            Ok(result) => result,
            Err(error) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(false, langgraph_tool_error(error)),
                );
            }
        };

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Handle `sentinel__get_phase_steps`
async fn handle_get_phase_steps(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let skill = match args.get("skill").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
            );
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'phase_id'")),
            );
        }
    };

    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };
    let session_id = { state.read().await.session_id.clone() };
    if !workflow_configs.contains_key(&skill) {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("No configured LangGraph workflow for '{skill}'")),
            ),
        );
    }
    let steps_config = match load_required_steps_config_for_rpc(request, &skill) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let graph_state =
        match graph_latest_workflow_state(&skill, &session_id, &workflow_configs).await {
            Ok(state) => state,
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!("phase graph checkpoint load failed: {e}")),
                    ),
                );
            }
        };
    let wf_state = graph_state.as_ref();
    let graph_topology = match graph_introspection(&skill, &session_id, &workflow_configs).await {
        Ok(topology) => topology,
        Err(e) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!("phase graph introspection failed: {e}")),
                ),
            );
        }
    };

    // Build step list — merge config definitions with runtime state
    let mut steps_list: Vec<serde_json::Value> = Vec::new();

    if let Some(phase_steps) = steps_config.phase_steps(&phase_id) {
        for step_def in &phase_steps.steps {
            // Find runtime state for this step
            let step_state = wf_state.and_then(|wf| {
                wf.step_states
                    .iter()
                    .find(|ss| ss.step_id == step_def.id && ss.phase_id == phase_id)
            });

            let status = step_state
                .map(|ss| &ss.status)
                .cloned()
                .unwrap_or(StepStatus::Pending);
            let summary = step_state.and_then(|ss| ss.summary.clone());

            let mut entry = serde_json::json!({
                "id": step_def.id,
                "description": step_def.description,
                "status": status,
                "blocker": step_def.blocker,
            });
            if let Some(sum) = summary {
                entry
                    .as_object_mut()
                    .unwrap()
                    .insert("summary".to_string(), serde_json::json!(sum));
            }
            steps_list.push(entry);
        }
    }

    let completed = wf_state.map_or(0, |w| w.phase_steps_completed(&phase_id));
    let total = steps_list.len();

    let mut result = serde_json::json!({
        "workflow_authority": "langgraph",
        "skill": skill,
        "phase_id": phase_id,
        "steps": steps_list,
        "completed": completed,
        "total": total,
        "graph_state": null,
    });
    if let (Some(obj), Some(topology)) = (result.as_object_mut(), graph_topology) {
        obj.insert("graph_topology".to_string(), serde_json::json!(topology));
    }
    if let Some(obj) = result.as_object_mut() {
        if let Err(error) =
            attach_phase_graph_read_evidence(obj, &skill, &session_id, &workflow_configs).await
        {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error(error)),
            );
        }
    }
    let result =
        match attach_workflow_read_graph_audit(WorkflowApiReadSurface::PhaseSteps, result).await {
            Ok(result) => result,
            Err(error) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(false, langgraph_tool_error(error)),
                );
            }
        };

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Handle `sentinel__get_workflow_progress`
async fn handle_get_workflow_progress(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let skill = match args.get("skill").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
            );
        }
    };

    let session_id = { state.read().await.session_id.clone() };
    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };
    if !workflow_configs.contains_key(&skill) {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("No configured LangGraph workflow for '{skill}'")),
            ),
        );
    }
    let steps_config = match load_required_steps_config_for_rpc(request, &skill) {
        Ok(config) => config,
        Err(response) => return response,
    };
    let graph_state =
        match graph_latest_workflow_state(&skill, &session_id, &workflow_configs).await {
            Ok(state) => state,
            Err(e) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!("phase graph checkpoint load failed: {e}")),
                    ),
                );
            }
        };
    let wf_state: Option<&WorkflowState> = graph_state.as_ref();
    let graph_topology = match graph_introspection(&skill, &session_id, &workflow_configs).await {
        Ok(topology) => topology,
        Err(e) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    langgraph_tool_error(format!("phase graph introspection failed: {e}")),
                ),
            );
        }
    };

    // Build phase-level progress
    let mut phases_list: Vec<serde_json::Value> = Vec::new();
    let mut overall_completed: usize = 0;
    let mut overall_total: usize = 0;

    let workflow = workflow_configs
        .get(&skill)
        .expect("workflow existence checked above");
    for phase in &workflow.phases {
        let phase_status = if wf_state.is_some_and(|w| w.is_phase_complete(&phase.id)) {
            "completed"
        } else if wf_state
            .is_some_and(|w| w.current_phase.is_some() && !w.completed_phases.contains(&phase.id))
            && wf_state.is_some_and(|w| {
                w.completed_phases.len()
                    == workflow
                        .phases
                        .iter()
                        .position(|p| p.id == phase.id)
                        .unwrap_or(0)
            })
        {
            "in_progress"
        } else {
            "pending"
        };

        // Step-level counts for this phase
        let steps_completed = wf_state.map_or(0, |w| w.phase_steps_completed(&phase.id));

        let steps_total = steps_config
            .phase_steps(&phase.id)
            .map_or(0, |ps| ps.steps.len());

        overall_completed += steps_completed;
        overall_total += steps_total;

        // Build step details for this phase
        let mut step_details: Vec<serde_json::Value> = Vec::new();
        if let Some(phase_steps) = steps_config.phase_steps(&phase.id) {
            for step_def in &phase_steps.steps {
                let step_state = wf_state.and_then(|wf| {
                    wf.step_states
                        .iter()
                        .find(|ss| ss.step_id == step_def.id && ss.phase_id == phase.id)
                });

                let st = step_state
                    .map(|ss| &ss.status)
                    .cloned()
                    .unwrap_or(StepStatus::Pending);

                step_details.push(serde_json::json!({
                    "id": step_def.id,
                    "description": step_def.description,
                    "status": st,
                }));
            }
        }

        let mut phase_entry = serde_json::json!({
            "id": phase.id,
            "description": phase.description,
            "status": phase_status,
            "steps_completed": steps_completed,
            "steps_total": steps_total,
        });

        if !step_details.is_empty() {
            phase_entry
                .as_object_mut()
                .unwrap()
                .insert("steps".to_string(), serde_json::json!(step_details));
        }

        phases_list.push(phase_entry);
    }

    let percentage = if overall_total > 0 {
        (overall_completed as f64 / overall_total as f64 * 100.0).round() as u32
    } else {
        0
    };

    let mut result = serde_json::json!({
        "workflow_authority": "langgraph",
        "skill": skill,
        "phases": phases_list,
        "overall": {
            "steps_completed": overall_completed,
            "steps_total": overall_total,
            "percentage": percentage,
        },
        "graph_state": null,
    });
    if let (Some(obj), Some(topology)) = (result.as_object_mut(), graph_topology) {
        obj.insert("graph_topology".to_string(), serde_json::json!(topology));
    }

    if let Some(obj) = result.as_object_mut() {
        if let Err(error) =
            attach_phase_graph_read_evidence(obj, &skill, &session_id, &workflow_configs).await
        {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, langgraph_tool_error(error)),
            );
        }
    }
    let result =
        match attach_workflow_read_graph_audit(WorkflowApiReadSurface::Progress, result).await {
            Ok(result) => result,
            Err(error) => {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(false, langgraph_tool_error(error)),
                );
            }
        };

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Handle `sentinel__replay_phase` — time-travel fork to re-run a phase.
async fn handle_replay_phase(
    request: &JsonRpcRequest,
    args: &serde_json::Value,
    state: &Arc<RwLock<SessionState>>,
) -> JsonRpcResponse {
    let Some(skill) = args.get("skill").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'skill'")),
        );
    };
    let Some(phase_id) = args.get("phase_id").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'phase_id'")),
        );
    };
    let Some(reason) = args.get("reason").and_then(|v| v.as_str()) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error("Missing 'reason'")),
        );
    };
    let workflow_configs = match load_workflow_configs_for_rpc(request) {
        Ok(configs) => configs,
        Err(response) => return response,
    };
    let Some(workflow) = workflow_configs.get(skill) else {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                langgraph_tool_error(format!("No workflow for '{skill}'")),
            ),
        );
    };
    let session_id = { state.read().await.session_id.clone() };

    let outcome = async {
        let db_path = phase_graph_db_path(&session_id)?;
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let forked = graph
            .replay_phase(skill, &session_id, phase_id, reason)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let phase_graph = phase_graph_mutation_evidence(&graph, &session_id, &forked).await?;
        Ok::<_, anyhow::Error>((forked, phase_graph))
    }
    .await;

    match outcome {
        Ok((forked, phase_graph)) => {
            let persist_result = {
                let mut s = state.write().await;
                s.set_active_skill_marker(skill);
                s.set_graph_projected_workflow(skill.to_string(), forked.to_workflow_state());
                sentinel_infrastructure::state_store::save(&mut s)
            };
            if let Err(e) = persist_result {
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(
                        false,
                        langgraph_tool_error(format!(
                            "failed to persist replayed graph state: {e}"
                        )),
                    ),
                );
            }
            JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(true, {
                    let mut result = serde_json::json!({
                    "skill": skill,
                    "replayed_phase": phase_id,
                    "current_phase": forked.current_phase,
                    "completed_phases": forked.completed_phases,
                    "current_step": forked.current_step,
                    "step_states": forked.step_states,
                    "phase_graph": phase_graph,
                    });
                    attach_phase_graph_response_authority(&mut result);
                    result
                }),
            )
        }
        Err(e) => JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(false, langgraph_tool_error(format!("replay failed: {e}"))),
        ),
    }
}

async fn attach_phase_graph_read_evidence(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    skill: &str,
    session_id: &str,
    workflow_configs: &HashMap<String, SkillWorkflow>,
) -> std::result::Result<(), String> {
    match graph_checkpoint_projection(skill, session_id, workflow_configs).await {
        Ok(Some(checkpoints)) => {
            attach_phase_graph_read_checkpoint_evidence(obj, checkpoints);
        }
        Ok(None) => {}
        Err(e) => {
            return Err(format!("phase graph checkpoint projection failed: {e}"));
        }
    }
    match graph_history_projection(skill, session_id, workflow_configs).await {
        Ok(Some(history)) => {
            obj.insert("graph_history".to_string(), history);
        }
        Ok(None) => {}
        Err(e) => {
            return Err(format!("phase graph history projection failed: {e}"));
        }
    }
    match graph_writes_projection(skill, session_id, workflow_configs, None).await {
        Ok(Some(writes)) => {
            obj.insert("graph_writes".to_string(), writes);
        }
        Ok(None) => {}
        Err(e) => {
            return Err(format!("phase graph write history projection failed: {e}"));
        }
    }

    Ok(())
}

async fn attach_workflow_read_graph_audit(
    surface: WorkflowApiReadSurface,
    mut response: serde_json::Value,
) -> std::result::Result<serde_json::Value, String> {
    let graph_audit =
        sentinel_infrastructure::workflow_api_read_graph::workflow_api_read_graph_audit(
            surface, &response,
        )
        .await?;
    response
        .as_object_mut()
        .ok_or_else(|| "workflow read graph audit can only attach to object responses".to_string())?
        .insert("graph_audit".to_string(), graph_audit);
    Ok(response)
}

/// Format MCP tool result in the standard content array format
fn mcp_tool_result(success: bool, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&data)
                .expect("MCP tool result JSON serialization must succeed")
        }],
        "isError": !success
    })
}

fn langgraph_tool_error(error: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "error": error.into(),
    })
}

fn attach_phase_graph_read_checkpoint_evidence(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    checkpoints: serde_json::Value,
) {
    if let Some(latest_checkpoint) = checkpoints
        .as_array()
        .and_then(|entries| entries.last())
        .cloned()
    {
        obj.insert("latest_checkpoint".to_string(), latest_checkpoint.clone());
        obj.insert(
            "graph_state".to_string(),
            latest_checkpoint
                .get("state")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        );
    }
    obj.insert("graph_checkpoints".to_string(), checkpoints);
}

fn attach_phase_graph_response_authority(response: &mut serde_json::Value) {
    let Some(obj) = response.as_object_mut() else {
        return;
    };
    obj.insert(
        "workflow_authority".to_string(),
        serde_json::json!("langgraph"),
    );
    let Some(phase_graph) = obj
        .get("phase_graph")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    let graph_state = phase_graph.get("graph_state").cloned();
    let latest_checkpoint = phase_graph.get("latest_checkpoint").cloned();
    if let Some(value) = graph_state {
        obj.insert("graph_state".to_string(), value);
    }
    if let Some(value) = latest_checkpoint {
        obj.insert("latest_checkpoint".to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::judge_service::JudgeService;
    use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
    use sentinel_domain::workflow::WorkflowPhase;
    use std::fs;
    use std::io::Write;

    /// Write a UUID-shaped stem for a given suffix char to make fixture
    /// session ids easy to tell apart.
    fn uuid_like(suffix: char) -> String {
        format!("11111111-2222-3333-4444-55555555555{suffix}")
    }

    fn phase_started_at() -> String {
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

    fn test_workflow(skill: &str) -> SkillWorkflow {
        SkillWorkflow {
            skill: skill.to_string(),
            phases: vec![
                WorkflowPhase {
                    id: "claim".to_string(),
                    file: "claim.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "claim".to_string(),
                    required_dyad: None,
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "fetch".to_string(),
                    required_dyad: None,
                },
            ],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    /// Make a project dir under `.claude/projects/<project>/` and write
    /// a transcript JSONL file named `<session_id>.jsonl` with the given
    /// JSON lines. Returns the transcript path.
    fn seed_transcript(
        home: &Path,
        project: &str,
        session_id: &str,
        lines: &[serde_json::Value],
    ) -> PathBuf {
        let dir = home.join(".claude").join("projects").join(project);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", serde_json::to_string(line).unwrap()).unwrap();
        }
        path
    }

    fn write_linear_step_config(config_dir: &Path) {
        let steps_dir = config_dir.join("steps");
        fs::create_dir_all(&steps_dir).unwrap();
        fs::write(
            steps_dir.join("linear.toml"),
            r#"
federation_version = "1"

[[phases]]
id = "claim"

[[phases.steps]]
id = "0.1"
description = "Collect the ticket claim"
blocker = true

[[phases]]
id = "fetch"

[[phases.steps]]
id = "1.1"
description = "Fetch supporting details"
blocker = true
"#,
        )
        .unwrap();
    }

    struct UnusedJudge;

    #[async_trait::async_trait]
    impl JudgeService for UnusedJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &sentinel_domain::evidence::Evidence,
            _model: JudgeModel,
        ) -> anyhow::Result<JudgeVerdict> {
            anyhow::bail!("test judge should not be called")
        }
    }

    struct PassingJudge;

    #[async_trait::async_trait]
    impl JudgeService for PassingJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &sentinel_domain::evidence::Evidence,
            _model: JudgeModel,
        ) -> anyhow::Result<JudgeVerdict> {
            Ok(JudgeVerdict::pass(0.95, "test pass"))
        }
    }

    fn checkpoint_snapshot(
        state: sentinel_graph::PhaseGraphState,
        checkpoint_id: &str,
    ) -> sentinel_graph::PhaseGraphCheckpointSnapshot {
        sentinel_graph::PhaseGraphCheckpointSnapshot {
            checkpoint_id: checkpoint_id.to_string(),
            parent_checkpoint_id: None,
            thread_id: "sentinel.phase.linear.test-session".to_string(),
            step_number: 1,
            created_at: "2026-06-17T00:00:00Z".to_string(),
            tags: std::collections::BTreeMap::new(),
            source: None,
            writes: vec![sentinel_graph::PhaseGraphCheckpointWrite {
                node_id: "claim".to_string(),
                channel: "state".to_string(),
                ts: "2026-06-17T00:00:00Z".to_string(),
            }],
            state,
        }
    }

    fn write_history_entry(
        state: &sentinel_graph::PhaseGraphState,
        checkpoint_id: &str,
    ) -> sentinel_graph::PhaseGraphWriteHistoryEntry {
        let value_json = serde_json::to_value(state).expect("state serializes");
        sentinel_graph::PhaseGraphWriteHistoryEntry {
            thread_id: format!("sentinel.phase.{}.{}", state.skill, state.session_id),
            checkpoint_id: checkpoint_id.to_string(),
            step_number: 1,
            channel: "state".to_string(),
            node_id: "claim".to_string(),
            ts: "2026-06-17T00:00:00Z".to_string(),
            value_len: serde_json::to_vec(&value_json)
                .expect("json serializes")
                .len(),
            value_sha256: "test-sha".to_string(),
            value_json,
        }
    }

    fn inject_old_workflows_field(
        state: &mut SessionState,
        skill: &str,
        workflow_state: WorkflowState,
    ) {
        let mut value = serde_json::to_value(&*state).expect("session state serializes");
        let old_workflows = serde_json::json!({
            skill: serde_json::to_value(workflow_state).expect("workflow state serializes")
        });
        value
            .as_object_mut()
            .expect("session state is an object")
            .insert("workflows".to_string(), old_workflows);
        *state = serde_json::from_value(value).expect("session state deserializes");
    }

    async fn seed_configured_phase_gate(skill: &str, session_id: &str) {
        let workflows = load_workflow_configs().expect("workflow config loads");
        let workflow = workflows
            .get(skill)
            .unwrap_or_else(|| panic!("workflow config missing skill {skill}"))
            .clone();
        let db_path = phase_graph_db_path(session_id).expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver)
            .expect("compile graph");
        graph
            .run_until_gate(skill, session_id)
            .await
            .expect("initial gate checkpoint");
    }

    #[test]
    fn mutation_evidence_requires_latest_checkpoint_state_match() {
        let mut projected =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        projected.current_phase = Some(1);
        projected.completed_phases = vec!["claim".into()];

        let mut stale = projected.clone();
        stale.current_phase = Some(0);
        stale.completed_phases.clear();

        let err = build_phase_graph_mutation_evidence(
            "test-session",
            &projected,
            vec![checkpoint_snapshot(stale, "checkpoint-1")],
            vec![write_history_entry(&projected, "checkpoint-1")],
            "sentinel.phase.linear.test-session",
        )
        .expect_err("stale latest checkpoint must fail closed");

        assert!(
            err.to_string().contains("latest checkpoint state mismatch"),
            "error must identify stale checkpoint state: {err:#}"
        );
    }

    #[test]
    fn mutation_evidence_requires_latest_checkpoint_state_write() {
        let mut projected =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        projected.current_phase = Some(1);
        projected.completed_phases = vec!["claim".into()];

        let err = build_phase_graph_mutation_evidence(
            "test-session",
            &projected,
            vec![checkpoint_snapshot(projected.clone(), "checkpoint-2")],
            vec![write_history_entry(&projected, "checkpoint-1")],
            "sentinel.phase.linear.test-session",
        )
        .expect_err("missing latest checkpoint write must fail closed");

        assert!(
            err.to_string()
                .contains("omitted latest checkpoint state-channel write"),
            "error must identify missing latest state write: {err:#}"
        );
    }

    #[test]
    fn mutation_evidence_requires_matching_write_thread() {
        let mut projected =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        projected.current_phase = Some(1);
        projected.completed_phases = vec!["claim".into()];
        let mut writes = vec![write_history_entry(&projected, "checkpoint-2")];
        writes[0].thread_id = "sentinel.phase.linear.other-session".to_string();

        let err = build_phase_graph_mutation_evidence(
            "test-session",
            &projected,
            vec![checkpoint_snapshot(projected.clone(), "checkpoint-2")],
            writes,
            "sentinel.phase.linear.test-session",
        )
        .expect_err("mismatched write thread must fail closed");

        assert!(
            err.to_string().contains("write history") && err.to_string().contains("other-session"),
            "error must identify mismatched write thread: {err:#}"
        );
    }

    #[test]
    fn mutation_evidence_requires_oldest_first_write_history() {
        let mut older =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        older.current_phase = Some(0);

        let mut projected =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        projected.current_phase = Some(1);
        projected.completed_phases = vec!["claim".into()];
        let mut older_checkpoint = checkpoint_snapshot(older.clone(), "checkpoint-1");
        older_checkpoint.step_number = 1;
        let mut latest_checkpoint = checkpoint_snapshot(projected.clone(), "checkpoint-2");
        latest_checkpoint.step_number = 2;
        let mut latest_write = write_history_entry(&projected, "checkpoint-2");
        latest_write.step_number = 2;
        let mut older_write = write_history_entry(&older, "checkpoint-1");
        older_write.step_number = 1;

        let err = build_phase_graph_mutation_evidence(
            "test-session",
            &projected,
            vec![older_checkpoint, latest_checkpoint],
            vec![latest_write, older_write],
            "sentinel.phase.linear.test-session",
        )
        .expect_err("out-of-order write history must fail closed");

        assert!(
            err.to_string().contains("write history")
                && err.to_string().contains("not oldest-first"),
            "error must identify out-of-order write history: {err:#}"
        );
    }

    #[test]
    fn mutation_evidence_requires_oldest_first_checkpoint_history() {
        let mut older =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        older.current_phase = Some(0);

        let mut projected =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        projected.current_phase = Some(1);
        projected.completed_phases = vec!["claim".into()];
        let mut latest_checkpoint = checkpoint_snapshot(projected.clone(), "checkpoint-2");
        latest_checkpoint.step_number = 2;
        let mut older_checkpoint = checkpoint_snapshot(older.clone(), "checkpoint-1");
        older_checkpoint.step_number = 1;

        let err = build_phase_graph_mutation_evidence(
            "test-session",
            &older,
            vec![latest_checkpoint, older_checkpoint],
            vec![
                write_history_entry(&projected, "checkpoint-2"),
                write_history_entry(&older, "checkpoint-1"),
            ],
            "sentinel.phase.linear.test-session",
        )
        .expect_err("out-of-order checkpoint history must fail closed");

        assert!(
            err.to_string().contains("not oldest-first"),
            "error must identify out-of-order checkpoint history: {err:#}"
        );
    }

    #[test]
    fn mutation_evidence_accepts_matching_latest_checkpoint_and_write() {
        let mut projected =
            sentinel_graph::PhaseGraphState::new("linear", "test-session", vec!["claim".into()]);
        projected.current_phase = Some(1);
        projected.completed_phases = vec!["claim".into()];

        let evidence = build_phase_graph_mutation_evidence(
            "test-session",
            &projected,
            vec![checkpoint_snapshot(projected.clone(), "checkpoint-3")],
            vec![write_history_entry(&projected, "checkpoint-3")],
            "sentinel.phase.linear.test-session",
        )
        .expect("matching checkpoint evidence is accepted");

        assert_eq!(evidence["workflow_authority"], "langgraph");
        assert_eq!(evidence["graph_state"], evidence["state"]);
        assert_eq!(evidence["state"]["current_phase"], 1);
        assert_eq!(
            evidence["latest_checkpoint"]["checkpoint_id"],
            "checkpoint-3"
        );
    }

    /// Run a closure with HOME (or USERPROFILE on win) pointed at a temp
    /// directory so the session detection code reads fixtures instead of
    /// the real user profile.
    fn with_fake_home<F: FnOnce(&Path) -> R, R>(f: F) -> R {
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("USERPROFILE", tmp.path());
        let result = f(tmp.path());
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        result
    }

    #[test]
    fn detect_live_session_picks_newest_mtime() {
        // Two sessions, b is newer → should win.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id_a = uuid_like('a');
            let id_b = uuid_like('b');
            seed_transcript(home, "proj1", &id_a, &[serde_json::json!({"type": "user"})]);
            // Sleep briefly so mtimes differ even on coarse FS clocks.
            std::thread::sleep(std::time::Duration::from_millis(20));
            seed_transcript(home, "proj1", &id_b, &[serde_json::json!({"type": "user"})]);
            assert_eq!(detect_live_session_id(), Some(id_b));
        });
        drop(lock);
    }

    #[test]
    fn detect_live_session_returns_none_without_transcripts() {
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|_home| {
            assert_eq!(detect_live_session_id(), None);
        });
        drop(lock);
    }

    #[test]
    fn detect_live_session_ignores_non_uuid_stems() {
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            // A bogus filename that is NOT a UUID — must be skipped even
            // if it's the newest file.
            let dir = home.join(".claude").join("projects").join("p");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("not-a-uuid.jsonl"), b"{}").unwrap();
            let id = uuid_like('c');
            std::thread::sleep(std::time::Duration::from_millis(20));
            seed_transcript(home, "p", &id, &[serde_json::json!({"type": "user"})]);
            assert_eq!(detect_live_session_id(), Some(id));
        });
        drop(lock);
    }

    #[test]
    fn session_id_by_tool_use_id_matches_correct_transcript() {
        // Two sessions, each with its own tool_use id. The lookup must
        // return the session whose transcript actually recorded the id
        // — not just the newest one.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id_a = uuid_like('a');
            let id_b = uuid_like('b');
            let tool_use_in_a = "toolu_A_only_this_one";
            let tool_use_in_b = "toolu_B_only_this_one";
            seed_transcript(
                home,
                "p",
                &id_a,
                &[serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": tool_use_in_a, "name": "Read", "input": {}}
                    ]}
                })],
            );
            // Make B newer, so newest-mtime would otherwise pick it —
            // we assert the toolUseId match overrides that heuristic.
            std::thread::sleep(std::time::Duration::from_millis(20));
            seed_transcript(
                home,
                "p",
                &id_b,
                &[serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": tool_use_in_b, "name": "Read", "input": {}}
                    ]}
                })],
            );

            // Looking up A's id → A must win, not B (even though B is newer).
            assert_eq!(session_id_by_tool_use_id(tool_use_in_a), Some(id_a));
            // And B's id still works.
            assert_eq!(session_id_by_tool_use_id(tool_use_in_b), Some(id_b));
        });
        drop(lock);
    }

    #[test]
    fn session_id_by_tool_use_id_returns_none_when_not_found() {
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id = uuid_like('a');
            seed_transcript(
                home,
                "p",
                &id,
                &[serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "toolu_real", "name": "Read", "input": {}}
                    ]}
                })],
            );
            assert_eq!(session_id_by_tool_use_id("toolu_nonexistent"), None);
        });
        drop(lock);
    }

    #[test]
    fn resolve_session_id_prefers_tool_use_id_over_newest_mtime() {
        // toolUseId points at an older session; newest-mtime is a different
        // one. The toolUseId signal must win because it's unambiguous.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let older_id = uuid_like('a');
            let newer_id = uuid_like('b');
            let tool_use_id = "toolu_owned_by_older";
            seed_transcript(
                home,
                "p",
                &older_id,
                &[serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "tool_use", "id": tool_use_id, "name": "Read", "input": {}}
                    ]}
                })],
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
            seed_transcript(home, "p", &newer_id, &[serde_json::json!({"type": "user"})]);

            let params = serde_json::json!({
                "_meta": {"claudecode/toolUseId": tool_use_id}
            });
            let resolved = resolve_session_id(&params).unwrap();
            assert_eq!(resolved, older_id, "toolUseId must win over newest-mtime");
        });
        drop(lock);
    }

    #[test]
    fn resolve_session_id_falls_back_to_newest_when_no_tool_use_id() {
        // No _meta — use newest-mtime.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id_a = uuid_like('a');
            let id_b = uuid_like('b');
            seed_transcript(home, "p", &id_a, &[serde_json::json!({"type": "user"})]);
            std::thread::sleep(std::time::Duration::from_millis(20));
            seed_transcript(home, "p", &id_b, &[serde_json::json!({"type": "user"})]);

            let params = serde_json::json!({});
            let resolved = resolve_session_id(&params).unwrap();
            assert_eq!(resolved, id_b);
        });
        drop(lock);
    }

    #[test]
    fn resolve_session_id_errors_when_tool_use_id_unknown() {
        // toolUseId supplied but not in any transcript: this is ambiguous
        // across concurrent Claude Code windows, so fail closed instead of
        // binding the request to a guessed session.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id = uuid_like('a');
            seed_transcript(home, "p", &id, &[serde_json::json!({"type": "user"})]);

            let params = serde_json::json!({
                "_meta": {"claudecode/toolUseId": "toolu_never_recorded"}
            });
            let err = resolve_session_id(&params).unwrap_err();
            assert!(err.to_string().contains("refusing to guess a session"));
        });
        drop(lock);
    }

    #[test]
    fn resolve_session_id_errors_when_no_session_at_all() {
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|_home| {
            let params = serde_json::json!({});
            assert!(resolve_session_id(&params).is_err());
        });
        drop(lock);
    }

    #[test]
    fn transcript_contains_tool_use_id_finds_nested_ids() {
        // Multiple tool_use blocks in one assistant message — the target
        // id may not be the first. Must still be detected.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id = uuid_like('a');
            let path = seed_transcript(
                home,
                "p",
                &id,
                &[serde_json::json!({
                    "type": "assistant",
                    "message": {"role": "assistant", "content": [
                        {"type": "text", "text": "thinking..."},
                        {"type": "tool_use", "id": "toolu_first", "name": "Read", "input": {}},
                        {"type": "tool_use", "id": "toolu_target", "name": "Edit", "input": {}}
                    ]}
                })],
            );
            assert!(transcript_contains_tool_use_id(&path, "toolu_target"));
            assert!(transcript_contains_tool_use_id(&path, "toolu_first"));
            assert!(!transcript_contains_tool_use_id(&path, "toolu_absent"));
        });
        drop(lock);
    }

    #[tokio::test]
    // ENV_LOCK is held across awaits deliberately: it serialises HOME/USERPROFILE
    // mutation between concurrent #[tokio::test]s, and the awaited transactions
    // run against that redirected env. Releasing the guard before the awaits
    // would let a sibling test race the env. Single-purpose test mutex, not a
    // runtime lock — the await-holding-lock concern doesn't apply.
    #[allow(clippy::await_holding_lock)]
    async fn with_session_state_loads_and_saves_through_lock() {
        // End-to-end: inside the lock, the handler sees state loaded for
        // the session; mutations are persisted and visible on a follow-up
        // call. Two sequential calls prove save+load round-trip across the
        // transaction boundary. state_store reads HOME via dirs::home_dir,
        // so redirecting HOME reroutes state too.
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("USERPROFILE", tmp.path());
        std::fs::create_dir_all(tmp.path().join(".claude").join("sentinel").join("state")).unwrap();

        let session_id = uuid_like('z');
        let handle = Arc::new(RwLock::new(SessionState::new("test-session-state")));
        let workflow_configs: HashMap<String, SkillWorkflow> = HashMap::new();

        // First transaction: set active_skill.
        let handle1 = handle.clone();
        with_session_state(&session_id, &handle, &workflow_configs, move || {
            let h = handle1;
            async move {
                let mut s = h.write().await;
                s.set_active_skill("my-skill");
            }
        })
        .await
        .unwrap();

        // Second transaction: expect active_skill to have persisted.
        let handle2 = handle.clone();
        with_session_state(&session_id, &handle, &workflow_configs, move || {
            let h = handle2;
            async move {
                let s = h.read().await;
                assert_eq!(s.active_skill.as_deref(), Some("my-skill"));
            }
        })
        .await
        .unwrap();

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn severity_graph_auditor_emits_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        let prev_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        let prev_pg_url = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        let prev_pg_schema = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");

        let proposal = sentinel_application::severity::SeverityProposal {
            issue_id: Some("linear-issue-id".to_string()),
            identifier: "FPCRM-777".to_string(),
            title: "Core workflow broken".to_string(),
            current_priority: Some(0),
            proposed_priority: 2,
            reasoning: "core workflow impact".to_string(),
            action: "set".to_string(),
            opus_priority: 2,
            gpt_priority: 2,
            models_agreed: true,
        };
        let graph_jsonl = tmp.path().join("severity.graph-runs.jsonl");

        let audit = CliSeverityGraphAuditor
            .audit_severity_proposals(&[proposal], &graph_jsonl)
            .await
            .expect("severity graph audit");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "severity");
        assert_eq!(audit.proposals_audited, 1);
        assert_eq!(audit.authorized_sets, 1);
        assert_eq!(audit.skipped, 0);
        assert_eq!(audit.runs[0].identifier, "FPCRM-777");
        assert_eq!(audit.runs[0].decision, "set");
        assert!(
            audit.runs[0].terminal_checkpoint.contains('#'),
            "severity audit must expose terminal graph checkpoint"
        );
        assert!(audit.runs[0]
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.runs[0].run["topology"]["graph"], "severity");
        assert_eq!(audit.runs[0].run["topology"]["durable_checkpointer"], true);
        assert!(
            audit.runs[0].run["checkpoints"]
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
            "severity audit must expose checkpoint history: {:?}",
            audit.runs[0].run
        );
        assert!(
            audit.runs[0].run["write_history"]
                .as_array()
                .is_some_and(|entries| entries.iter().any(|entry| entry["channel"] == "state")),
            "severity audit must expose state write history: {:?}",
            audit.runs[0].run
        );
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"severity\""));

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        match prev_backend {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
        }
        match prev_pg_url {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
        }
        match prev_pg_schema {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn with_session_state_projects_langgraph_checkpoint_after_load() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("HOME", tmp.path());
        std::env::set_var("USERPROFILE", tmp.path());
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::fs::create_dir_all(tmp.path().join(".claude").join("sentinel").join("state")).unwrap();

        let session_id = uuid_like('y');
        let wf = test_workflow("linear");
        let db_path = phase_graph_db_path(&session_id).expect("phase graph db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(&wf, saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", &session_id)
            .await
            .expect("initial graph gate");
        graph
            .apply_verdict("linear", &session_id, "claim", true)
            .await
            .expect("claim verdict");

        let workflow_configs = HashMap::from([("linear".to_string(), wf)]);
        let handle = Arc::new(RwLock::new(SessionState::new("empty-before-load")));
        let handle_for_closure = handle.clone();
        let expected_session_id = session_id.clone();

        with_session_state(&session_id, &handle, &workflow_configs, move || {
            let h = handle_for_closure.clone();
            let expected_session_id = expected_session_id.clone();
            async move {
                let state = h.read().await;
                let projected = state
                    .graph_workflow("linear")
                    .expect("workflow projected from checkpoint");
                assert_eq!(projected.session_id, expected_session_id);
                assert_eq!(projected.completed_phases, vec!["claim".to_string()]);
                assert_eq!(projected.current_phase, Some(1));
            }
        })
        .await
        .unwrap();

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn graph_latest_workflow_state_reads_durable_checkpoint() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"

[[workflows.phases]]
id = "fetch"
file = "fetch.md"
required = true
judge = "sonnet"
description = "Fetch"
"#,
        )
        .unwrap();

        let session_id = "graph-read-authority";
        let workflow = load_workflow_configs()
            .expect("workflow config loads")
            .remove("linear")
            .expect("workflow from temp config");
        let db_path = phase_graph_db_path(session_id).unwrap();
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .unwrap();
        let graph =
            sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver).unwrap();
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");
        graph
            .apply_verdict("linear", session_id, "claim", true)
            .await
            .expect("persist graph checkpoint");

        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let projected = graph_latest_workflow_state("linear", session_id, &workflow_configs)
            .await
            .expect("load graph latest")
            .expect("configured workflow yields state");
        assert_eq!(projected.current_phase, Some(1));
        assert_eq!(projected.completed_phases, vec!["claim".to_string()]);

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    async fn submit_phase_requires_summary() {
        let state = Arc::new(RwLock::new(SessionState::new("missing-phase-summary")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(PassingJudge)).with_signing(None, false),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "started_at": phase_started_at()
        });

        let resp = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "missing phase summary must fail");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("Missing 'summary'")),
            "error must name missing summary: {data}"
        );
    }

    #[tokio::test]
    async fn submit_phase_requires_started_at() {
        let state = Arc::new(RwLock::new(SessionState::new("missing-phase-start")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(PassingJudge)).with_signing(None, false),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "summary": "claim is complete"
        });

        let resp = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "missing phase started_at must fail");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("Missing 'started_at'")),
            "error must name missing started_at: {data}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn submit_phase_success_returns_langgraph_stream_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"

[[workflows.phases]]
id = "fetch"
file = "fetch.md"
required = true
judge = "sonnet"
description = "Fetch"
"#,
        )
        .unwrap();

        seed_configured_phase_gate("linear", "stream-submit-session").await;
        let state = Arc::new(RwLock::new(SessionState::new("stream-submit-session")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(PassingJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "summary": "claim is complete",
            "started_at": phase_started_at()
        });

        let resp = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "submit should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["status"], "accepted");
        assert_eq!(data["completed_phases"], serde_json::json!(["claim"]));

        let phase_graph = data.get("phase_graph").expect("phase_graph evidence");
        assert_eq!(data["graph_state"], phase_graph["graph_state"]);
        assert_eq!(data["latest_checkpoint"], phase_graph["latest_checkpoint"]);
        assert_eq!(phase_graph["state"]["current_phase"], 1);
        assert!(
            phase_graph["latest_checkpoint"]["checkpoint_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "phase submit must expose durable LangGraph checkpoint evidence: {phase_graph}"
        );
        assert!(
            phase_graph["checkpoints"]
                .as_array()
                .is_some_and(|checkpoints| !checkpoints.is_empty()),
            "phase submit must expose LangGraph checkpoint history: {phase_graph}"
        );
        assert!(
            phase_graph["writes"]
                .as_array()
                .expect("writes array")
                .iter()
                .any(|write| write["channel"] == "state"),
            "phase submit must expose LangGraph state-channel writes: {phase_graph}"
        );
        let topology = phase_graph
            .get("graph_topology")
            .expect("phase submit must expose compiled graph topology");
        assert_eq!(
            topology["thread_id"],
            "sentinel.phase.linear.stream-submit-session"
        );
        assert!(
            topology["checkpointer_backend"]
                .as_str()
                .is_some_and(|backend| !backend.is_empty()),
            "phase submit topology must expose the checkpointer backend: {topology}"
        );
        assert!(
            topology["checkpointer_scope"]
                .as_str()
                .is_some_and(|scope| !scope.is_empty()),
            "phase submit topology must expose the sanitized checkpointer scope: {topology}"
        );
        let stream = phase_graph["stream"]
            .as_array()
            .expect("phase_graph.stream array");
        assert!(
            !stream.is_empty(),
            "non-terminal submit must expose LangGraph re-entry stream"
        );
        assert!(
            stream.iter().any(|part| part["payload_kind"] == "values"),
            "MCP response must include LangGraph values stream payloads: {phase_graph}"
        );
        assert!(
            stream.iter().any(|part| {
                part["payload_kind"] == "checkpoints"
                    && part["payload_json"]
                        .pointer("/source/type")
                        .and_then(serde_json::Value::as_str)
                        == Some("stream_update")
                    && part["payload_json"]
                        .pointer("/source/node")
                        .and_then(serde_json::Value::as_str)
                        == Some("fetch")
            }),
            "MCP response must include the next-gate checkpoint stream: {phase_graph}"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    async fn generic_handler_errors_do_not_claim_workflow_authority() {
        let session_id = "authority-error-session";
        let state = Arc::new(RwLock::new(SessionState::new(session_id)));
        {
            let mut s = state.write().await;
            s.restore_proof_chain(
                "linear".to_string(),
                sentinel_domain::proof::ProofChain::new("linear", session_id),
            );
        }

        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__get_proof_chain",
                "arguments": {"skill": "linear"},
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);

        assert!(is_error, "graphless proof read must fail: {data}");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("LangGraph-projected workflow state")),
            "error must preserve lower-level graph authority failure detail: {data}"
        );
    }

    #[tokio::test]
    async fn tools_call_rejects_missing_tool_name_before_dispatch() {
        let state = Arc::new(RwLock::new(SessionState::new("missing-tool-name")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({}),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let err = resp
            .error
            .expect("missing tool name must be JSON-RPC error");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("params.name"),
            "error must name missing params.name: {}",
            err.message
        );
        assert!(resp.result.is_none());
    }

    #[tokio::test]
    async fn tools_call_rejects_malformed_arguments_before_dispatch() {
        let state = Arc::new(RwLock::new(SessionState::new("malformed-tool-args")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__get_workflow_status",
                "arguments": []
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let err = resp
            .error
            .expect("malformed arguments must be JSON-RPC error");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("params.arguments"),
            "error must name malformed arguments: {}",
            err.message
        );
        assert!(resp.result.is_none());
    }

    #[tokio::test]
    async fn edit_claude_md_template_rejects_missing_find_before_operational_audit() {
        let state = Arc::new(RwLock::new(SessionState::new("missing-template-find")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__edit_claude_md_template",
                "arguments": {"replace": "replacement text"}
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let err = resp.error.expect("missing find must be JSON-RPC error");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("find"),
            "error must name missing find: {}",
            err.message
        );
        assert!(resp.result.is_none());
    }

    #[tokio::test]
    async fn edit_claude_md_template_rejects_missing_replace_before_operational_audit() {
        let state = Arc::new(RwLock::new(SessionState::new("missing-template-replace")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__edit_claude_md_template",
                "arguments": {"find": "unique text"}
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let err = resp.error.expect("missing replace must be JSON-RPC error");
        assert_eq!(err.code, -32602);
        assert!(
            err.message.contains("replace"),
            "error must name missing replace: {}",
            err.message
        );
        assert!(resp.result.is_none());
    }

    #[tokio::test]
    async fn delegate_kimi_requires_content_before_worker_setup() {
        let state = Arc::new(RwLock::new(SessionState::new("kimi-missing-content")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__delegate_kimi_context_scan",
                "arguments": {"question": "Which facts matter?"}
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "missing content must be rejected: {data}");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("content")),
            "error must name missing content: {data}"
        );
    }

    #[tokio::test]
    async fn delegate_codex_rejects_non_string_context_before_worker_setup() {
        let state = Arc::new(RwLock::new(SessionState::new("codex-bad-context")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__delegate_codex",
                "arguments": {
                    "task": "review this design",
                    "context": []
                }
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "non-string context must be rejected: {data}");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("context")),
            "error must name malformed context: {data}"
        );
    }

    #[tokio::test]
    async fn delegate_codex_rejects_malformed_max_tokens_before_worker_setup() {
        let state = Arc::new(RwLock::new(SessionState::new("codex-bad-max-tokens")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__delegate_codex",
                "arguments": {
                    "task": "review this design",
                    "max_tokens": 0
                }
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "bad max_tokens must be rejected: {data}");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("max_tokens")),
            "error must name malformed max_tokens: {data}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn duplicate_phase_submit_rejected_before_second_proof() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"

[[workflows.phases]]
id = "fetch"
file = "fetch.md"
required = true
judge = "sonnet"
description = "Fetch"
"#,
        )
        .unwrap();

        seed_configured_phase_gate("linear", "duplicate-submit-session").await;
        let state = Arc::new(RwLock::new(SessionState::new("duplicate-submit-session")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(PassingJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "summary": "claim is complete",
            "started_at": phase_started_at()
        });

        let first = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (first_data, first_error) = extract_data_and_is_error(&first);
        assert!(!first_error, "first submit should succeed: {first_data}");

        let second = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (second_data, second_error) = extract_data_and_is_error(&second);
        assert!(second_error, "duplicate submit must fail: {second_data}");
        assert!(
            second_data
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| {
                    err.contains("rejected by phase graph")
                        && err.contains("already completed")
                        && err.contains("replay")
                }),
            "duplicate error must come from graph authority: {second_data}"
        );

        let state = state.read().await;
        let chain = state.proof_chain("linear").expect("proof chain");
        assert_eq!(
            chain.phase_count(),
            1,
            "duplicate graph rejection must not append a second phase proof"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn submit_phase_unknown_phase_does_not_mutate_session_state() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("phase-test-session")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "ghost",
            "summary": "not real",
            "started_at": phase_started_at()
        });

        let resp = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "unknown phase must return an MCP error");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("Unknown phase 'ghost'")),
            "error should name the rejected phase: {data}"
        );

        let state = state.read().await;
        assert!(
            !state.has_any_graph_workflow(),
            "unknown phase must not create workflow state"
        );
        assert!(
            state.phases_read.is_empty(),
            "unknown phase must not create phase-read evidence"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn submit_phase_graph_preflight_failure_does_not_mutate_session_state() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let sentinel_dir = tmp.path().join(".claude").join("sentinel");
        let config_dir = sentinel_dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state_dir = sentinel_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("phase-graphs"), b"not a directory").unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("graph-preflight-fails")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "summary": "valid phase but graph store is unavailable",
            "started_at": phase_started_at()
        });

        let resp = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "graph preflight failure must return an MCP error");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err
                    .contains("phase graph checkpoint load failed before proof submission")),
            "error should report graph preflight failure: {data}"
        );

        let state = state.read().await;
        assert!(
            state.active_skill.is_none(),
            "graph preflight failure must not set active skill"
        );
        assert!(
            !state.has_any_graph_workflow(),
            "graph preflight failure must not create workflow state"
        );
        assert!(
            state.phases_read.is_empty(),
            "graph preflight failure must not create phase-read evidence"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn submit_phase_without_checkpoint_fails_closed_and_ignores_old_workflows_field() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"

[[workflows.phases]]
id = "fetch"
file = "fetch.md"
required = true
judge = "sonnet"
description = "Fetch"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("stale-submit-session")));
        {
            let mut s = state.write().await;
            let mut forged = WorkflowState::new("linear", "stale-submit-session");
            forged.completed_phases = vec!["claim".to_string()];
            forged.current_phase = Some(1);
            inject_old_workflows_field(&mut s, "linear", forged);
        }
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(PassingJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "fetch",
            "summary": "try to skip claim using stale cache",
            "started_at": phase_started_at()
        });

        let resp = handle_submit_phase(&req, &args, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "phase submit without graph gate must fail");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("requires an existing LangGraph checkpoint")),
            "error should require durable graph gate checkpoint: {data}"
        );

        let state = state.read().await;
        assert!(
            state.graph_workflow("linear").is_none(),
            "no-checkpoint submit must ignore old workflows progress"
        );
        assert!(
            !state.has_any_graph_workflow(),
            "no-checkpoint submit must not create workflow projections"
        );
        assert!(
            state.phases_read.is_empty(),
            "no-checkpoint submit must not create phase-read evidence"
        );
        assert!(
            state.proof_chain("linear").is_none(),
            "graph-rejected submit must not seal a proof"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_workflow_progress_includes_compiled_graph_topology() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"

[[workflows.phases]]
id = "fetch"
file = "fetch.md"
required = true
judge = "sonnet"
description = "Fetch"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);

        let state = Arc::new(RwLock::new(SessionState::new("progress-topology-session")));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({ "skill": "linear" });

        let resp = handle_get_workflow_progress(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "progress should succeed: {data}");

        let topology = &data["graph_topology"];
        assert_eq!(
            topology["thread_id"],
            "sentinel.phase.linear.progress-topology-session"
        );
        assert_eq!(
            topology["phase_order"],
            serde_json::json!(["claim", "fetch"])
        );
        assert_eq!(
            topology["schemas"]["state"]["properties"]["phase_order"]["const"],
            serde_json::json!(["claim", "fetch"])
        );
        assert_eq!(
            topology["schemas"]["state"]["x-sentinel"]["authority"],
            "langgraph"
        );
        let nodes = topology["nodes"].as_array().expect("topology nodes");
        let claim = nodes
            .iter()
            .find(|node| node["id"] == "claim")
            .expect("claim topology node");
        assert_eq!(claim["metadata"]["sentinel.phase"], "claim");
        assert_eq!(claim["has_timeout_policy"], true);
        assert_eq!(claim["interrupt_after"], true);
        assert!(
            topology["edges"]
                .as_array()
                .expect("topology edges")
                .iter()
                .any(|edge| edge["from"] == "claim" && edge["kind"] == "conditional"),
            "topology must expose LangGraph conditional routing"
        );
        assert_workflow_read_graph_audit(&data, "progress");
        assert_workflow_api_read_jsonl(tmp.path(), "progress");

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_workflow_progress_returns_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);

        let session_id = "workflow-progress-evidence";
        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path(session_id).expect("phase graph db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver).unwrap();
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");
        graph
            .apply_verdict("linear", session_id, "claim", true)
            .await
            .expect("phase completion checkpoint");

        let state = Arc::new(RwLock::new(SessionState::new(session_id)));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({ "skill": "linear" });

        let resp = handle_get_workflow_progress(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "workflow progress should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["skill"], "linear");
        assert_eq!(data["phases"][0]["status"], "completed");
        assert_eq!(
            data["graph_state"]["completed_phases"],
            serde_json::json!(["claim"])
        );
        assert_eq!(
            data["latest_checkpoint"]["checkpoint_id"],
            data["graph_checkpoints"]
                .as_array()
                .expect("checkpoint history")
                .last()
                .expect("latest checkpoint")["checkpoint_id"]
        );
        assert_eq!(
            data["graph_topology"]["thread_id"],
            "sentinel.phase.linear.workflow-progress-evidence"
        );
        assert!(
            data["graph_writes"]
                .as_array()
                .expect("write history")
                .iter()
                .any(|write| write["channel"] == "state"
                    && write["value_json"]["completed_phases"] == serde_json::json!(["claim"])),
            "workflow progress must expose LangGraph state-channel write evidence: {data}"
        );
        assert_workflow_read_graph_audit(&data, "progress");
        assert_workflow_api_read_jsonl(tmp.path(), "progress");

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_workflow_progress_rejects_unconfigured_old_workflows_field() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("old-ghost-progress")));
        {
            let mut state = state.write().await;
            let mut forged = WorkflowState::new("ghost", "old-ghost-progress");
            forged.completed_phases = vec!["claim".to_string()];
            forged.current_phase = Some(1);
            inject_old_workflows_field(&mut state, "ghost", forged);
        }
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({ "skill": "ghost" });

        let resp = handle_get_workflow_progress(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "unconfigured old workflow field must be rejected");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("No configured LangGraph workflow")),
            "error must name missing graph workflow: {data}"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_phase_steps_rejects_unconfigured_old_workflows_field() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("old-ghost-steps")));
        {
            let mut state = state.write().await;
            let mut forged = WorkflowState::new("ghost", "old-ghost-steps");
            forged.update_step(
                "claim",
                "0.1",
                StepStatus::Completed,
                Some("forged step".to_string()),
            );
            inject_old_workflows_field(&mut state, "ghost", forged);
        }
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({ "skill": "ghost", "phase_id": "claim" });

        let resp = handle_get_phase_steps(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "unconfigured old step state must be rejected");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("No configured LangGraph workflow")),
            "error must name missing graph workflow: {data}"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_phase_steps_returns_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);
        let steps_config = sentinel_infrastructure::config::load_skill_steps(&config_dir, "linear")
            .expect("step config loads")
            .expect("linear step config");
        let step_policy = steps_config
            .phase_steps("claim")
            .and_then(|phase| phase.steps.iter().find(|step| step.id == "0.1"))
            .expect("claim step policy")
            .clone();

        let session_id = "phase-steps-evidence";
        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path(session_id).expect("phase graph db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver).unwrap();
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");
        graph
            .update_step(
                "linear",
                session_id,
                "claim",
                "0.1",
                &step_policy,
                StepStatus::InProgress,
                Some("step underway".to_string()),
            )
            .await
            .expect("step update checkpoint");

        let state = Arc::new(RwLock::new(SessionState::new(session_id)));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({ "skill": "linear", "phase_id": "claim" });

        let resp = handle_get_phase_steps(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "phase steps should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["skill"], "linear");
        assert_eq!(data["phase_id"], "claim");
        assert_eq!(data["steps"][0]["id"], "0.1");
        assert_eq!(data["steps"][0]["status"], "in_progress");
        assert_eq!(data["graph_state"]["step_states"][0]["step_id"], "0.1");
        assert_eq!(
            data["latest_checkpoint"]["checkpoint_id"],
            data["graph_checkpoints"]
                .as_array()
                .expect("checkpoint history")
                .last()
                .expect("latest checkpoint")["checkpoint_id"]
        );
        assert_eq!(
            data["graph_topology"]["thread_id"],
            "sentinel.phase.linear.phase-steps-evidence"
        );
        assert!(
            data["graph_writes"]
                .as_array()
                .expect("write history")
                .iter()
                .any(|write| write["channel"] == "state"
                    && write["value_json"]["step_states"][0]["step_id"] == "0.1"),
            "phase steps must expose LangGraph state-channel write evidence: {data}"
        );
        assert_workflow_read_graph_audit(&data, "phase_steps");
        assert_workflow_api_read_jsonl(tmp.path(), "phase_steps");

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn update_step_nonterminal_returns_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);

        let state = Arc::new(RwLock::new(SessionState::new("update-step-evidence")));
        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path("update-step-evidence").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", "update-step-evidence")
            .await
            .expect("initial gate checkpoint");
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "step_id": "0.1",
            "status": "in_progress",
            "summary": "step underway"
        });

        let resp = handle_update_step(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "step update should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        let phase_graph = data.get("phase_graph").expect("phase_graph evidence");
        assert_eq!(data["graph_state"], phase_graph["graph_state"]);
        assert_eq!(data["latest_checkpoint"], phase_graph["latest_checkpoint"]);
        assert_eq!(phase_graph["state"]["step_states"][0]["step_id"], "0.1");
        assert_eq!(
            phase_graph["state"]["step_states"][0]["status"],
            "in_progress"
        );
        assert!(
            phase_graph["latest_checkpoint"]["checkpoint_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "latest checkpoint must identify the durable graph mutation: {phase_graph}"
        );
        assert!(
            phase_graph["writes"]
                .as_array()
                .expect("writes array")
                .iter()
                .any(|write| write["channel"] == "state"),
            "step update must expose LangGraph state-channel writes: {phase_graph}"
        );
        let topology = phase_graph
            .get("graph_topology")
            .expect("step update must expose compiled graph topology");
        assert_eq!(
            topology["thread_id"],
            "sentinel.phase.linear.update-step-evidence"
        );
        assert!(
            topology["checkpointer_backend"]
                .as_str()
                .is_some_and(|backend| !backend.is_empty()),
            "step update topology must expose the checkpointer backend: {topology}"
        );
        assert!(
            topology["checkpointer_scope"]
                .as_str()
                .is_some_and(|scope| !scope.is_empty()),
            "step update topology must expose the sanitized checkpointer scope: {topology}"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn update_step_without_gate_checkpoint_fails_closed() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);

        let state = Arc::new(RwLock::new(SessionState::new("update-step-no-gate")));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "step_id": "0.1",
            "status": "in_progress",
            "summary": "step underway"
        });

        let resp = handle_update_step(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "step update without graph gate must fail");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("requires an existing checkpoint")),
            "error must identify missing LangGraph checkpoint: {data}"
        );

        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let latest =
            graph_latest_workflow_state("linear", "update-step-no-gate", &workflow_configs)
                .await
                .expect("graph lookup should succeed");
        assert!(
            latest.is_none(),
            "rejected update must not create a graph checkpoint"
        );
        assert!(
            !state.read().await.has_any_graph_workflow(),
            "rejected update must not cache workflow state"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn update_step_rejects_terminal_status_without_step_proof() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new(
            "update-step-terminal-rejected",
        )));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "step_id": "0.1",
            "status": "completed",
            "summary": "proofless completion"
        });

        let resp = handle_update_step(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "proofless terminal update must fail");
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| {
                    err.contains("sentinel__submit_step_complete") && err.contains("StepProof")
                }),
            "error must direct callers to graph-backed proof sealing: {data}"
        );

        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let latest = graph_latest_workflow_state(
            "linear",
            "update-step-terminal-rejected",
            &workflow_configs,
        )
        .await
        .expect("graph lookup should succeed");
        assert!(
            latest.is_none(),
            "rejected terminal update must not persist a graph checkpoint"
        );
        let state = state.read().await;
        assert!(
            !state.has_any_graph_workflow(),
            "rejected terminal update must not cache workflow state"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn submit_step_complete_returns_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);

        let workflows = load_workflow_configs().expect("workflow config loads");
        let step_configs = load_step_configs_for_workflows(&workflows).expect("step config loads");
        let workflow = workflows.get("linear").expect("linear workflow");
        let state = Arc::new(RwLock::new(SessionState::new("submit-step-evidence")));
        let db_path = phase_graph_db_path("submit-step-evidence").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", "submit-step-evidence")
            .await
            .expect("initial gate checkpoint");
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge))
                .with_signing(None, false)
                .with_step_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone())
            .with_workflows(workflows)
            .with_step_configs(step_configs);
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "step_id": "0.1",
            "step_description": "collect the ticket claim",
            "summary": "claim step complete",
            "verdict": {
                "sufficient": true,
                "confidence": 0.93,
                "reasoning": "evidence satisfies the step"
            },
            "started_at": Utc::now().to_rfc3339(),
            "evidence": empty_evidence_json(),
            "artifact": {"ticket": "FPCRM-1"}
        });
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__submit_step_complete",
                "arguments": args,
            }),
        };

        state
            .write()
            .await
            .record_independent_verdict("linear", "claim", "0.1", true, 0.93);
        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "step submit should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["status"], "accepted");
        assert_eq!(data["proof"]["step_id"], "0.1");
        assert_eq!(data["proof"]["phase_id"], "claim");
        assert_eq!(data["graph_state"], data["phase_graph"]["graph_state"]);
        assert_eq!(
            data["latest_checkpoint"],
            data["phase_graph"]["latest_checkpoint"]
        );
        assert_eq!(
            data["phase_graph"]["state"]["step_states"][0]["step_id"],
            "0.1"
        );
        assert_eq!(
            data["phase_graph"]["state"]["step_states"][0]["status"],
            "completed"
        );
        assert!(
            data["phase_graph"]["latest_checkpoint"]["checkpoint_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty()),
            "step submit must expose durable LangGraph checkpoint evidence: {data}"
        );
        assert!(
            data["phase_graph"]["writes"]
                .as_array()
                .expect("writes array")
                .iter()
                .any(|write| write["channel"] == "state"),
            "step submit must expose LangGraph state-channel writes: {data}"
        );

        let state = state.read().await;
        let chain = state.proof_chain("linear").expect("proof chain");
        assert_eq!(chain.entries.len(), 1);
        let workflow = state
            .graph_workflow("linear")
            .expect("graph-projected state");
        assert_eq!(workflow.step_states[0].step_id, "0.1");
        assert_eq!(workflow.step_states[0].status, StepStatus::Completed);

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn duplicate_step_submit_rejected_before_second_step_proof() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);

        let workflows = load_workflow_configs().expect("workflow config loads");
        let step_configs = load_step_configs_for_workflows(&workflows).expect("step config loads");
        let workflow = workflows.get("linear").expect("linear workflow");
        let state = Arc::new(RwLock::new(SessionState::new("duplicate-step-submit")));
        let db_path = phase_graph_db_path("duplicate-step-submit").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", "duplicate-step-submit")
            .await
            .expect("initial gate checkpoint");
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge))
                .with_signing(None, false)
                .with_step_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let handler = McpHandler::new(state.clone(), proof_engine)
            .with_workflows(workflows)
            .with_step_configs(step_configs);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "step_id": "0.1",
            "step_description": "collect the ticket claim",
            "summary": "claim step complete",
            "verdict": {
                "sufficient": true,
                "confidence": 0.93,
                "reasoning": "evidence satisfies the step"
            },
            "started_at": Utc::now().to_rfc3339(),
            "evidence": empty_evidence_json(),
            "artifact": {"ticket": "FPCRM-1"}
        });

        state
            .write()
            .await
            .record_independent_verdict("linear", "claim", "0.1", true, 0.93);
        let first = handle_submit_step_complete(&req, &args, &state, &handler).await;
        let (first_data, first_is_error) = extract_data_and_is_error(&first);
        assert!(
            !first_is_error,
            "initial step submit should succeed: {first_data}"
        );
        let checkpoint_count = first_data["phase_graph"]["checkpoints"]
            .as_array()
            .expect("checkpoint array")
            .len();

        let second = handle_submit_step_complete(&req, &args, &state, &handler).await;
        let (second_data, second_is_error) = extract_data_and_is_error(&second);
        assert!(
            second_is_error,
            "duplicate step submit must fail closed: {second_data}"
        );
        assert!(second_data.get("workflow_authority").is_none());
        assert!(
            second_data
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| {
                    err.contains("LangGraph step authority failed")
                        && err.contains("already terminal")
                        && err.contains("replay the phase")
                }),
            "duplicate error should come from graph terminal-step authority: {second_data}"
        );

        let workflow_configs = load_workflow_configs().expect("workflow config reloads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path("duplicate-step-submit").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .expect("compile graph");
        assert_eq!(
            graph
                .phase_snapshots("duplicate-step-submit")
                .await
                .expect("snapshots")
                .len(),
            checkpoint_count,
            "duplicate step submit must not append a graph checkpoint"
        );

        let state = state.read().await;
        let chain = state.proof_chain("linear").expect("proof chain");
        assert_eq!(
            chain.entries.len(),
            1,
            "duplicate step submit must not seal a second StepProof"
        );
        let workflow = state
            .graph_workflow("linear")
            .expect("graph-projected state");
        assert_eq!(workflow.step_states.len(), 1);
        assert_eq!(workflow.step_states[0].status, StepStatus::Completed);

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn submit_step_complete_graph_preflight_failure_does_not_seal_proof() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let sentinel_dir = tmp.path().join(".claude").join("sentinel");
        let config_dir = sentinel_dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();
        write_linear_step_config(&config_dir);
        let state_dir = sentinel_dir.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("phase-graphs"), b"not a directory").unwrap();

        let workflows = load_workflow_configs().expect("workflow config loads");
        let step_configs = load_step_configs_for_workflows(&workflows).expect("step config loads");
        let state = Arc::new(RwLock::new(SessionState::new("submit-step-graph-fails")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge))
                .with_signing(None, false)
                .with_step_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let handler = McpHandler::new(state.clone(), proof_engine)
            .with_workflows(workflows)
            .with_step_configs(step_configs);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "step_id": "0.1",
            "step_description": "collect the ticket claim",
            "verdict": {
                "sufficient": true,
                "confidence": 0.93,
                "reasoning": "evidence satisfies the step"
            },
            "started_at": Utc::now().to_rfc3339(),
            "evidence": empty_evidence_json()
        });

        state
            .write()
            .await
            .record_independent_verdict("linear", "claim", "0.1", true, 0.93);
        let resp = handle_submit_step_complete(&req, &args, &state, &handler).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "graph preflight failure must return an MCP error");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("LangGraph step authority failed")),
            "error should report graph preflight failure: {data}"
        );

        let state = state.read().await;
        assert!(
            state.proof_chains_is_empty(),
            "graph preflight failure must not seal a StepProof"
        );
        assert!(
            !state.has_any_graph_workflow(),
            "graph preflight failure must not create workflow state"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_workflow_status_returns_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let session_id = "workflow-status-checkpoint";
        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path(session_id).expect("phase graph db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver).unwrap();
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");

        let state = Arc::new(RwLock::new(SessionState::new(session_id)));
        {
            let mut s = state.write().await;
            let mut forged = WorkflowState::new("linear", session_id);
            forged.completed_phases = vec!["forged".to_string()];
            inject_old_workflows_field(&mut s, "linear", forged);
        }
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__get_workflow_status",
                "arguments": {"skill": "linear"},
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);

        assert!(!is_error, "workflow status should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["skill"], "linear");
        assert_eq!(data["current_phase"], 0);
        assert!(data["completed_phases"].as_array().unwrap().is_empty());
        assert_workflow_read_graph_audit(&data, "status");
        assert_eq!(data["graph_state"], data["latest_checkpoint"]["state"]);
        assert!(
            data["graph_topology"]["thread_id"]
                .as_str()
                .is_some_and(|thread| thread == "sentinel.phase.linear.workflow-status-checkpoint"),
            "workflow status must expose graph topology: {data}"
        );
        assert!(
            data["graph_checkpoints"]
                .as_array()
                .expect("graph checkpoints")
                .iter()
                .any(|checkpoint| checkpoint["state"]["skill"] == "linear"),
            "workflow status must expose checkpoint history: {data}"
        );
        assert!(
            data["graph_history"]
                .as_array()
                .expect("graph history")
                .iter()
                .any(|state| state["skill"] == "linear"),
            "workflow status must expose state-only phase history: {data}"
        );
        assert_eq!(
            data["graph_history"].as_array().unwrap().len(),
            data["graph_checkpoints"].as_array().unwrap().len()
        );
        assert!(
            data["graph_writes"]
                .as_array()
                .expect("graph writes")
                .iter()
                .any(|write| write["channel"] == "state"),
            "workflow status must expose state-channel writes: {data}"
        );
        assert!(
            data["completed_phases"]
                .as_array()
                .unwrap()
                .iter()
                .all(|phase| phase != "forged"),
            "workflow status must ignore stale local workflow fields: {data}"
        );
        assert_workflow_api_read_jsonl(tmp.path(), "status");

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_workflow_status_without_checkpoint_reports_graph_no_checkpoint() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("workflow-status-empty")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__get_workflow_status",
                "arguments": {"skill": "linear"},
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);

        assert!(
            !is_error,
            "no-checkpoint status is a graph read state: {data}"
        );
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["skill"], "linear");
        assert_eq!(data["status"], "no_checkpoint");
        assert_workflow_read_graph_audit(&data, "status");
        assert!(data["graph_state"].is_null());
        assert!(data.get("latest_checkpoint").is_none());
        assert!(data.get("graph_history").is_none());
        assert!(
            data["graph_topology"]["thread_id"]
                .as_str()
                .is_some_and(|thread| thread == "sentinel.phase.linear.workflow-status-empty"),
            "no-checkpoint status still exposes graph topology: {data}"
        );
        assert!(
            !state.read().await.has_any_graph_workflow(),
            "read-only no-checkpoint status must not synthesize workflow state"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_session_stats_reports_only_durable_langgraph_workflows() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let session_id = "stats-session";
        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path(session_id).expect("phase graph db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver).unwrap();
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");

        let state = Arc::new(RwLock::new(SessionState::new(session_id)));
        {
            let mut s = state.write().await;
            let mut forged = WorkflowState::new("old-state", "stats-session");
            forged.completed_phases = vec!["claim".to_string()];
            forged.current_phase = Some(1);
            inject_old_workflows_field(&mut s, "old-state", forged);
            s.set_graph_projected_workflow("ghost", WorkflowState::new("ghost", "stats-session"));
        }
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__get_session_stats",
                "arguments": {},
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);

        assert!(!is_error, "stats should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["langgraph_workflow_count"], 1);
        assert_eq!(data["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(data["graph_audit"]["graph"], "session_stats");
        assert_eq!(data["graph_audit"]["decision"], "verified");
        assert!(data["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|value| value.contains('#')));
        assert_eq!(
            data["graph_audit"]["run"]["topology"]["graph"],
            "session_stats"
        );
        assert!(data.get("workflows").is_none());
        let workflows = data["langgraph_workflows"]
            .as_array()
            .expect("langgraph_workflows array");
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0], "linear");
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("session-stats.graph-runs.jsonl"),
        )
        .expect("session stats graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn get_session_stats_projection_failure_does_not_claim_workflow_authority() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("../invalid-session")));
        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(UnusedJudge)).with_signing(None, false),
        );
        let handler = McpHandler::new(state.clone(), proof_engine.clone());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__get_session_stats",
                "arguments": {},
            }),
        };

        let resp = handle_request(&req, &handler, &state, &proof_engine).await;
        let (data, is_error) = extract_data_and_is_error(&resp);

        assert!(
            is_error,
            "projection failure should be an MCP tool error: {data}"
        );
        assert!(
            data["error"]
                .as_str()
                .is_some_and(|error| error.contains("phase graph checkpoint projection failed")),
            "unexpected error payload: {data}"
        );
        assert!(data.get("workflow_authority").is_none());
        assert!(data.get("graph_audit").is_none());

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn record_dyad_verdict_persists_graph_checkpoint_and_unlocks_phase_submit() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
required_dyad = { reviewer = true, tester = false }
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("dyad-mcp-session")));
        let workflows = load_workflow_configs().expect("workflow config loads");
        let workflow = workflows.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path("dyad-mcp-session").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .expect("compile graph");
        graph
            .run_until_gate("linear", "dyad-mcp-session")
            .await
            .expect("initial gate checkpoint");
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };

        let implementer_args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "role": "implementer",
            "agent": "builder-agent"
        });
        let implementer_resp = handle_record_dyad_verdict(&req, &implementer_args, &state).await;
        let (implementer_data, implementer_error) = extract_data_and_is_error(&implementer_resp);
        assert!(
            !implementer_error,
            "implementer dyad update should succeed: {implementer_data}"
        );

        let reviewer_args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "role": "reviewer",
            "agent": "reviewer-agent"
        });
        let reviewer_resp = handle_record_dyad_verdict(&req, &reviewer_args, &state).await;
        let (reviewer_data, reviewer_error) = extract_data_and_is_error(&reviewer_resp);
        assert!(
            !reviewer_error,
            "reviewer dyad update should succeed: {reviewer_data}"
        );
        assert_eq!(
            reviewer_data["dyad_verdict"]["implementer"],
            "builder-agent"
        );
        assert_eq!(
            reviewer_data["dyad_verdict"]["reviewer_pass_by"],
            "reviewer-agent"
        );
        assert_eq!(reviewer_data["workflow_authority"], "langgraph");
        let phase_graph = reviewer_data
            .get("phase_graph")
            .expect("phase graph evidence");
        assert_eq!(reviewer_data["graph_state"], phase_graph["graph_state"]);
        assert_eq!(
            reviewer_data["latest_checkpoint"],
            phase_graph["latest_checkpoint"]
        );
        assert_eq!(
            phase_graph["state"]["dyad_verdicts"]["claim"]["reviewer_pass_by"],
            "reviewer-agent"
        );
        assert!(
            phase_graph["writes"]
                .as_array()
                .expect("writes array")
                .iter()
                .any(|write| write["channel"] == "state"),
            "dyad update must expose LangGraph state-channel writes: {phase_graph}"
        );

        {
            let state = state.read().await;
            let wf = state
                .graph_workflow("linear")
                .expect("dyad graph projection");
            let dyad = wf.dyad_verdicts.get("claim").expect("dyad verdict");
            assert_eq!(dyad.implementer.as_deref(), Some("builder-agent"));
            assert_eq!(dyad.reviewer_pass_by.as_deref(), Some("reviewer-agent"));
        }

        let proof_engine = Arc::new(
            ProofEngine::new(state.clone(), Arc::new(PassingJudge))
                .with_signing(None, false)
                .with_phase_graph_authority(Arc::new(CliPhaseGraphAuthority)),
        );
        let submit_args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "summary": "claim completed with reviewer dyad",
            "started_at": phase_started_at()
        });
        let submit_resp = handle_submit_phase(&req, &submit_args, &state, &proof_engine).await;
        let (submit_data, submit_error) = extract_data_and_is_error(&submit_resp);
        assert!(
            !submit_error,
            "phase submit should use checkpointed dyad authorization: {submit_data}"
        );
        assert_eq!(
            submit_data["completed_phases"],
            serde_json::json!(["claim"])
        );
        let checkpoint_count = submit_data["phase_graph"]["checkpoints"]
            .as_array()
            .expect("checkpoint array")
            .len();

        let late_reviewer_args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "role": "reviewer",
            "agent": "late-reviewer-agent"
        });
        let late_reviewer_resp =
            handle_record_dyad_verdict(&req, &late_reviewer_args, &state).await;
        let (late_reviewer_data, late_reviewer_error) =
            extract_data_and_is_error(&late_reviewer_resp);
        assert!(
            late_reviewer_error,
            "late dyad update must fail after phase completion: {late_reviewer_data}"
        );
        assert!(
            late_reviewer_data
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| {
                    err.contains("dyad verdict graph update failed")
                        && err.contains("already completed")
                        && err.contains("replay the phase")
                }),
            "late dyad error should come from graph sealed-phase authority: {late_reviewer_data}"
        );

        let workflow_configs = load_workflow_configs().expect("workflow config reloads");
        let workflow = workflow_configs.get("linear").expect("linear workflow");
        let db_path = phase_graph_db_path("dyad-mcp-session").expect("db path");
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .expect("phase saver");
        let graph = sentinel_graph::compile_skill_graph_with_checkpointer(workflow, saver)
            .expect("compile graph");
        assert_eq!(
            graph
                .phase_snapshots("dyad-mcp-session")
                .await
                .expect("snapshots")
                .len(),
            checkpoint_count,
            "late dyad update must not append a graph checkpoint"
        );
        {
            let state = state.read().await;
            let wf = state
                .graph_workflow("linear")
                .expect("dyad graph projection");
            let dyad = wf.dyad_verdicts.get("claim").expect("dyad verdict");
            assert_eq!(dyad.reviewer_pass_by.as_deref(), Some("reviewer-agent"));
        }

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn record_dyad_verdict_without_gate_checkpoint_fails_closed() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"
required_dyad = { reviewer = true, tester = false }
"#,
        )
        .unwrap();

        let state = Arc::new(RwLock::new(SessionState::new("dyad-no-gate")));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "claim",
            "role": "implementer",
            "agent": "builder-agent"
        });

        let resp = handle_record_dyad_verdict(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "dyad update without graph gate must fail");
        assert!(data.get("workflow_authority").is_none());
        assert!(
            data.get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|err| err.contains("requires an existing checkpoint")),
            "error must identify missing LangGraph checkpoint: {data}"
        );

        let workflow_configs = load_workflow_configs().expect("workflow config loads");
        let latest = graph_latest_workflow_state("linear", "dyad-no-gate", &workflow_configs)
            .await
            .expect("graph lookup should succeed");
        assert!(
            latest.is_none(),
            "rejected dyad update must not create a graph checkpoint"
        );
        assert!(
            !state.read().await.has_any_graph_workflow(),
            "rejected dyad update must not cache workflow state"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn replay_phase_returns_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());

        let config_dir = tmp.path().join(".claude").join("sentinel").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("workflows.toml"),
            r#"
[[workflows]]
skill = "linear"

[[workflows.phases]]
id = "claim"
file = "claim.md"
required = true
judge = "sonnet"
description = "Claim"

[[workflows.phases]]
id = "fetch"
file = "fetch.md"
required = true
judge = "sonnet"
description = "Fetch"
"#,
        )
        .unwrap();

        let session_id = "replay-evidence-session";
        let workflow = load_workflow_configs()
            .expect("workflow config loads")
            .remove("linear")
            .expect("workflow from temp config");
        let db_path = phase_graph_db_path(session_id).unwrap();
        let saver = sentinel_graph::phase_checkpointer_from_env(&db_path)
            .await
            .unwrap();
        let graph =
            sentinel_graph::compile_skill_graph_with_checkpointer(&workflow, saver).unwrap();
        graph
            .run_until_gate("linear", session_id)
            .await
            .expect("initial gate checkpoint");
        graph
            .apply_verdict("linear", session_id, "claim", true)
            .await
            .expect("claim checkpoint");
        graph
            .apply_verdict("linear", session_id, "fetch", true)
            .await
            .expect("fetch checkpoint");

        let state = Arc::new(RwLock::new(SessionState::new(session_id)));
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };
        let args = serde_json::json!({
            "skill": "linear",
            "phase_id": "fetch",
            "reason": "QA requested fetch replay"
        });

        let resp = handle_replay_phase(&req, &args, &state).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "replay should succeed: {data}");
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["current_phase"], 1);
        assert_eq!(data["completed_phases"], serde_json::json!(["claim"]));
        let phase_graph = data.get("phase_graph").expect("phase_graph evidence");
        assert_eq!(data["graph_state"], phase_graph["graph_state"]);
        assert_eq!(data["latest_checkpoint"], phase_graph["latest_checkpoint"]);
        assert_eq!(phase_graph["state"]["current_phase"], 1);
        assert_eq!(
            phase_graph["state"]["completed_phases"],
            serde_json::json!(["claim"])
        );
        assert_eq!(
            phase_graph["state"]["replay_events"][0]["reason"],
            "QA requested fetch replay"
        );
        assert_eq!(
            phase_graph["state"]["replay_events"][0]["superseded_completed_phases"],
            serde_json::json!(["fetch"])
        );
        assert!(
            phase_graph["latest_checkpoint"]["parent_checkpoint_id"].is_string(),
            "replay checkpoint must preserve LangGraph lineage: {phase_graph}"
        );
        assert!(
            phase_graph["writes"]
                .as_array()
                .expect("writes array")
                .iter()
                .any(|write| write["channel"] == "state"),
            "replay must expose LangGraph state-channel writes: {phase_graph}"
        );

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        drop(env_guard);
    }

    /// Tests mutate process env (`HOME/USERPROFILE/SENTINEL_STATE_DIR`) and
    /// must serialize — cargo test runs in parallel by default.
    struct EnvLock;

    static ENV_LOCK: EnvLock = EnvLock;

    impl EnvLock {
        fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'static, ()>> {
            Ok(crate::test_env::lock())
        }
    }

    #[test]
    fn mcp_required_step_config_missing_is_langgraph_error() {
        let _env = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::Value::Null,
        };

        let response = match load_required_steps_config_for_rpc(&req, "linear") {
            Ok(_) => panic!("missing required step config must fail"),
            Err(response) => response,
        };
        let result = response.result.expect("tool result");
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().expect("text result");
        let data: serde_json::Value = serde_json::from_str(text).expect("json text");

        assert!(data.get("workflow_authority").is_none());
        let error = data["error"].as_str().expect("error");
        assert!(error.contains("configured LangGraph workflow 'linear'"));
        assert!(error.contains("missing required step config"));
        assert!(error.contains("steps/linear.toml"));

        match prev_sentinel_home {
            Some(value) => std::env::set_var("SENTINEL_HOME", value),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
    }

    #[test]
    fn mcp_signing_key_loader_requires_key() {
        let _env = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("SENTINEL_SIGNING_KEY");
        std::env::remove_var("SENTINEL_SIGNING_KEY");

        let err = load_signing_key_from_env().expect_err("missing signing key must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("SENTINEL_SIGNING_KEY") && msg.contains("required"),
            "error must name required signing key: {msg}"
        );

        match prev {
            Some(value) => std::env::set_var("SENTINEL_SIGNING_KEY", value),
            None => std::env::remove_var("SENTINEL_SIGNING_KEY"),
        }
    }

    #[test]
    fn mcp_verify_key_loader_requires_valid_key() {
        let _env = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("SENTINEL_VERIFY_KEY");
        std::env::remove_var("SENTINEL_VERIFY_KEY");

        let missing = load_verify_key_from_env().expect_err("missing verify key must fail");
        let missing_msg = format!("{missing:#}");
        assert!(
            missing_msg.contains("SENTINEL_VERIFY_KEY") && missing_msg.contains("required"),
            "missing-key error must name required verify key: {missing_msg}"
        );

        std::env::set_var("SENTINEL_VERIFY_KEY", "not-hex");
        let malformed = load_verify_key_from_env().expect_err("malformed verify key must fail");
        let malformed_msg = format!("{malformed:#}");
        assert!(
            malformed_msg.contains("valid hex"),
            "malformed-key error must reject non-hex input: {malformed_msg}"
        );

        match prev {
            Some(value) => std::env::set_var("SENTINEL_VERIFY_KEY", value),
            None => std::env::remove_var("SENTINEL_VERIFY_KEY"),
        }
    }

    #[test]
    fn mcp_key_loaders_accept_matching_ed25519_keypair() {
        let _env = ENV_LOCK.lock().unwrap();
        let prev_signing = std::env::var_os("SENTINEL_SIGNING_KEY");
        let prev_verify = std::env::var_os("SENTINEL_VERIFY_KEY");

        let seed = [7_u8; 32];
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        std::env::set_var("SENTINEL_SIGNING_KEY", hex::encode(seed));
        std::env::set_var(
            "SENTINEL_VERIFY_KEY",
            hex::encode(signing.verifying_key().as_bytes()),
        );

        let loaded_signing = load_signing_key_from_env().expect("signing key");
        let loaded_verify = load_verify_key_from_env().expect("verify key");
        assert_eq!(loaded_signing.verifying_key(), loaded_verify);

        match prev_signing {
            Some(value) => std::env::set_var("SENTINEL_SIGNING_KEY", value),
            None => std::env::remove_var("SENTINEL_SIGNING_KEY"),
        }
        match prev_verify {
            Some(value) => std::env::set_var("SENTINEL_VERIFY_KEY", value),
            None => std::env::remove_var("SENTINEL_VERIFY_KEY"),
        }
    }

    // ---- A2 Phase 5: route_capability MCP tool ----

    fn route_capability_request(requirement: serde_json::Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__route_capability",
                "arguments": { "requirement": requirement },
            }),
        }
    }

    fn extract_result(response: &JsonRpcResponse) -> &serde_json::Value {
        response.result.as_ref().expect("expected success response")
    }

    /// Extract the inner data JSON from the wrapped `mcp_tool_result`
    /// response. `mcp_tool_result(success, data)` wraps `data` as the
    /// stringified contents of `content[0].text` and sets `isError =
    /// !success` on the outer object. Tests assert against the outer
    /// `isError` flag and the parsed inner data.
    fn extract_data_and_is_error(response: &JsonRpcResponse) -> (serde_json::Value, bool) {
        let result = extract_result(response);
        let is_error = result
            .get("isError")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let data: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
        (data, is_error)
    }

    fn assert_workflow_read_graph_audit(data: &serde_json::Value, surface: &str) {
        assert_eq!(data["graph_audit"]["workflow_authority"], "langgraph");
        assert_eq!(data["graph_audit"]["graph"], "workflow_api_read");
        assert_eq!(data["graph_audit"]["surface"], surface);
        assert_eq!(data["graph_audit"]["decision"], "verified");
        assert!(
            data["graph_audit"]["authorization_checkpoint"]
                .as_str()
                .is_some_and(|checkpoint| checkpoint.contains('#')),
            "workflow read graph audit must expose checkpoint evidence: {data}"
        );
    }

    fn assert_workflow_api_read_jsonl(sentinel_home: &Path, surface: &str) {
        let graph_rows = fs::read_to_string(
            sentinel_home
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("workflow-api-read.graph-runs.jsonl"),
        )
        .expect("workflow API read graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"workflow_api_read\""));
        assert!(
            graph_rows.contains(&format!("\"surface\":\"{surface}\"")),
            "workflow API read graph rows must include surface {surface}: {graph_rows}"
        );
    }

    #[tokio::test]
    async fn route_capability_handles_missing_requirement() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: "tools/call".to_string(),
            params: serde_json::json!({
                "name": "sentinel__route_capability",
                "arguments": {},
            }),
        };
        let args = req.params.get("arguments").cloned().unwrap_or_default();
        let resp = handle_route_capability(&req, &args).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "missing requirement should flag isError=true");
        let err = data.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("requirement"),
            "error should name the missing field; got: {err}"
        );
    }

    #[tokio::test]
    async fn route_capability_handles_malformed_requirement() {
        let req = route_capability_request(serde_json::json!({
            "required": "not-an-array",
        }));
        let args = req.params.get("arguments").cloned().unwrap_or_default();
        let resp = handle_route_capability(&req, &args).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(is_error, "malformed requirement should flag isError=true");
        let err = data.get("error").and_then(|e| e.as_str()).unwrap_or("");
        assert!(
            err.contains("CapabilityRequirement") || err.contains("parse"),
            "error should mention CapabilityRequirement / parse: {err}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn route_capability_returns_routing_explanation_for_valid_requirement() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        let prev_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        let prev_pg_url = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        let prev_pg_schema = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");

        // Shipped agents-defaults.toml includes a Standard-reasoning
        // non-Anthropic profile (kimi-k2-6-ollama-cloud), so this
        // requirement should resolve to a chosen agent.
        let req = route_capability_request(serde_json::json!({
            "required": [
                { "Reasoning": "standard" },
                { "DifferentVendorFrom": "Anthropic" },
                { "StructuredOutput": "AuditorVerdict" },
            ],
            "preferred": [],
            "forbidden": [],
        }));
        let args = req.params.get("arguments").cloned().unwrap_or_default();
        let resp = handle_route_capability(&req, &args).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(!is_error, "valid requirement should succeed; data: {data}");
        // RoutingExplanation shape: { chosen, candidates, eliminated,
        // tie_breakers_applied, requirement_signature }.
        assert!(
            data.get("chosen").is_some(),
            "RoutingExplanation must have chosen field"
        );
        assert!(data.get("candidates").is_some());
        assert!(data.get("requirement_signature").is_some());
        let candidates = data.get("candidates").unwrap().as_array().unwrap();
        assert!(
            !candidates.is_empty(),
            "shipped defaults should provide at least one non-Anthropic Standard candidate"
        );
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["graph_audit"]["graph"], "capability_route");
        assert!(data["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("capability-route.graph-runs.jsonl"),
        )
        .expect("capability route graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"capability_route\""));

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        match prev_backend {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
        }
        match prev_pg_url {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
        }
        match prev_pg_schema {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn route_capability_returns_chosen_null_when_no_agent_satisfies() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        let prev_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        let prev_pg_url = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        let prev_pg_schema = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");

        // Require capabilities no shipped profile satisfies:
        // OpenWeights + Catastrophic-class qualification AND a custom
        // schema none of the shipped profiles declare.
        let req = route_capability_request(serde_json::json!({
            "required": [
                "OpenWeights",
                { "ReversibilityClass": "Catastrophic" },
                { "StructuredOutput": { "Named": "TotallyMadeUp" } },
            ],
            "preferred": [],
            "forbidden": [],
        }));
        let args = req.params.get("arguments").cloned().unwrap_or_default();
        let resp = handle_route_capability(&req, &args).await;
        let (data, is_error) = extract_data_and_is_error(&resp);
        assert!(
            !is_error,
            "explain() always succeeds (even when chosen=null)"
        );
        assert!(
            data.get("chosen").is_some_and(serde_json::Value::is_null),
            "no agent should satisfy contrived requirement; data: {data}"
        );
        assert_eq!(data["workflow_authority"], "langgraph");
        assert_eq!(data["graph_audit"]["decision"], "no-route");
        assert!(data["graph_audit"]["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        match prev_backend {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
        }
        match prev_pg_url {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
        }
        match prev_pg_schema {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn delegation_graph_audit_emits_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        let prev_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        let prev_pg_url = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        let prev_pg_schema = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");

        let request = sentinel_application::delegation_service::DelegationRequest {
            worker: sentinel_application::delegation_service::Worker::Codex,
            task: "review this patch".to_string(),
            context: "diff --git a/lib.rs b/lib.rs".to_string(),
            max_tokens: 128,
        };
        let result = sentinel_application::delegation_service::DelegationResult {
            worker: "codex".to_string(),
            output: "the patch keeps the invariant intact".to_string(),
        };
        let audit = run_delegation_graph_audit(&request, &result)
            .await
            .expect("delegation graph audit");
        assert_eq!(audit["workflow_authority"], "langgraph");
        assert_eq!(audit["graph"], "delegation");
        assert_eq!(audit["decision"], "completed");
        assert!(audit["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit["run"]["topology"]["durable_checkpointer"], true);
        assert_eq!(audit["run"]["topology"]["checkpointer_backend"], "sqlite");
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("delegation.graph-runs.jsonl"),
        )
        .expect("delegation graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"delegation\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        match prev_backend {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
        }
        match prev_pg_url {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
        }
        match prev_pg_schema {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA"),
        }
        drop(env_guard);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn wip_snapshot_graph_audit_emits_langgraph_checkpoint_evidence() {
        let env_guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        let prev_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        let prev_pg_url = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        let prev_pg_schema = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");

        let response = serde_json::json!({
            "captured_at": null,
            "message": "no snapshot captured yet - poller has not run"
        });
        let audit = run_wip_snapshot_graph_audit(&response)
            .await
            .expect("wip snapshot graph audit");
        assert_eq!(audit["workflow_authority"], "langgraph");
        assert_eq!(audit["graph"], "wip_snapshot");
        assert_eq!(audit["decision"], "no-snapshot");
        assert!(audit["authorization_checkpoint"]
            .as_str()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit["run"]["topology"]["durable_checkpointer"], true);
        assert_eq!(audit["run"]["topology"]["checkpointer_backend"], "sqlite");
        let graph_rows = std::fs::read_to_string(
            tmp.path()
                .join(".claude")
                .join("sentinel")
                .join("metrics")
                .join("wip-snapshot.graph-runs.jsonl"),
        )
        .expect("wip snapshot graph audit rows");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"wip_snapshot\""));
        assert!(graph_rows.contains("\"authorization_checkpoint\""));

        match prev_sentinel_home {
            Some(v) => std::env::set_var("SENTINEL_HOME", v),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        match prev_backend {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
        }
        match prev_pg_url {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL"),
        }
        match prev_pg_schema {
            Some(v) => std::env::set_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA", v),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA"),
        }
        drop(env_guard);
    }

    #[test]
    fn route_capability_is_listed_in_tool_definitions() {
        let defs = tool_definitions();
        let tools = defs.get("tools").unwrap().as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(
            names.contains(&"sentinel__route_capability"),
            "route_capability must appear in tool_definitions; got: {names:?}"
        );
        assert!(
            names.contains(&"sentinel__submit_step_complete"),
            "submit_step_complete must appear in tool_definitions; got: {names:?}"
        );
        let submit_phase = tools
            .iter()
            .find(|tool| {
                tool.get("name").and_then(|name| name.as_str())
                    == Some("sentinel__submit_phase_complete")
            })
            .expect("submit_phase_complete definition");
        let submit_phase_required = submit_phase
            .pointer("/inputSchema/required")
            .and_then(serde_json::Value::as_array)
            .expect("submit_phase_complete required array");
        assert!(
            submit_phase_required
                .iter()
                .any(|field| field.as_str() == Some("started_at")),
            "submit_phase_complete must require caller-supplied started_at; schema: {submit_phase}"
        );
        assert!(
            submit_phase_required
                .iter()
                .any(|field| field.as_str() == Some("summary")),
            "submit_phase_complete must require caller-supplied summary; schema: {submit_phase}"
        );
        let submit_step = tools
            .iter()
            .find(|tool| {
                tool.get("name").and_then(|name| name.as_str())
                    == Some("sentinel__submit_step_complete")
            })
            .expect("submit_step_complete definition");
        assert!(
            submit_step
                .pointer("/inputSchema/properties/judge_model")
                .is_none(),
            "submit_step_complete must not accept caller-supplied judge_model; schema: {submit_step}"
        );
        let submit_step_required = submit_step
            .pointer("/inputSchema/required")
            .and_then(serde_json::Value::as_array)
            .expect("submit_step_complete required array");
        assert!(
            submit_step_required
                .iter()
                .any(|field| field.as_str() == Some("started_at")),
            "submit_step_complete must require caller-supplied started_at; schema: {submit_step}"
        );
        assert!(
            submit_step_required
                .iter()
                .any(|field| field.as_str() == Some("evidence")),
            "submit_step_complete must require explicit structured evidence; schema: {submit_step}"
        );
        for required in [
            "sentinel__tokens_scan",
            "sentinel__cache_efficiency",
            "sentinel__cost_per_point",
            "sentinel__deploy_frequency",
            "sentinel__pr_review",
            "sentinel__roi",
            "sentinel__sla",
            "sentinel__eval_run",
            "sentinel__ba_draft",
        ] {
            assert!(
                names.contains(&required),
                "{required} must appear in tool_definitions; got: {names:?}"
            );
        }
    }
}
