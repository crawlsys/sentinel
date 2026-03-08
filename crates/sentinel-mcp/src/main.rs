//! Sentinel MCP Server — built with Vulcan SDK
//!
//! Standalone MCP server exposing proof chains, workflow tracking,
//! and session stats. Claude Code connects via stdio transport.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{info, warn};
use vulcan::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::tool::ToolRouter,
    model::{ServerInfo, ProtocolVersion, ServerCapabilities, Implementation},
    tool, tool_handler, tool_router,
    transport::stdio,
};

use sentinel_application::judge_service::{FallbackJudge, JudgeService};
use sentinel_application::proof_engine::ProofEngine;
use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::JudgeModel;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, StepStatus, WorkflowState};

// ============================================================================
// Server struct
// ============================================================================

#[derive(Clone)]
struct SentinelMcp {
    state: Arc<RwLock<SessionState>>,
    proof_engine: Arc<ProofEngine>,
    tool_router: ToolRouter<Self>,
}

impl SentinelMcp {
    fn new() -> Self {
        let session_id = std::env::var("SESSION_ID")
            .or_else(|_| std::env::var("CLAUDE_SESSION_ID"))
            .unwrap_or_else(|_| format!("mcp-{}", Utc::now().timestamp()));

        let state = if let Ok(Some(existing)) = sentinel_infrastructure::state_store::load(&session_id) {
            info!(session_id = %session_id, "Loaded existing session state");
            Arc::new(RwLock::new(existing))
        } else {
            info!(session_id = %session_id, "Creating new session state");
            Arc::new(RwLock::new(SessionState::new(&session_id)))
        };

        let judge: Arc<dyn JudgeService> = {
            let multi = sentinel_infrastructure::rig_judge::MultiModelJudge::from_env();
            if multi.has_any_provider() {
                Arc::new(multi)
            } else {
                warn!("No AI judge providers available — using blocking fallback");
                Arc::new(FallbackJudge)
            }
        };

        let proof_engine = Arc::new(ProofEngine::new(state.clone(), judge));

        Self {
            state,
            proof_engine,
            tool_router: Self::tool_router(),
        }
    }
}

// ============================================================================
// Tool implementations
// ============================================================================

#[tool_router]
impl SentinelMcp {
    /// Get the cryptographic proof chain for a skill execution.
    /// Returns all phase proofs with tessera hashes, evidence, and judge verdicts.
    #[tool(description = "Get the cryptographic proof chain for a skill execution. Returns all phase proofs with tessera hashes, evidence, and judge verdicts.")]
    async fn get_proof_chain(&self, skill: String) -> Result<String, ErrorData> {
        let state = self.state.read().await;
        match state.proof_chains.get(&skill) {
            Some(chain) => serde_json::to_string_pretty(chain)
                .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None)),
            None => Err(ErrorData::invalid_params(
                format!("No proof chain for skill '{skill}'"),
                None,
            )),
        }
    }

    /// Get the current workflow state for a skill.
    /// Shows which phases are completed, current phase, and what's next.
    #[tool(description = "Get the current workflow state for a skill. Shows which phases are completed, current phase, and what's next.")]
    async fn get_workflow_status(&self, skill: String) -> Result<String, ErrorData> {
        let state = self.state.read().await;
        match state.workflows.get(&skill) {
            Some(wf) => serde_json::to_string_pretty(wf)
                .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None)),
            None => Err(ErrorData::invalid_params(
                format!("No workflow state for skill '{skill}'"),
                None,
            )),
        }
    }

    /// Re-verify the integrity of a skill's proof chain.
    /// Checks all hashes are consistent and no tampering has occurred.
    #[tool(description = "Re-verify the integrity of a skill's proof chain. Checks all hashes are consistent and no tampering has occurred.")]
    async fn verify_chain(&self, skill: String) -> Result<String, ErrorData> {
        match self.proof_engine.verify_chain(&skill).await {
            Ok(verification) => serde_json::to_string_pretty(&verification)
                .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None)),
            Err(e) => Err(ErrorData::internal_error(
                format!("Verification failed: {e}"),
                None,
            )),
        }
    }

    /// Notify sentinel that a skill phase has been completed.
    /// Sentinel will evaluate the evidence and add a proof to the chain if sufficient.
    #[tool(description = "Notify sentinel that a skill phase has been completed. Sentinel will evaluate the evidence and add a proof to the chain if sufficient.")]
    async fn submit_phase_complete(
        &self,
        skill: String,
        phase_id: String,
        summary: String,
    ) -> Result<String, ErrorData> {
        // Record phase read in state
        let phase_file = format!("{phase_id}.md");
        {
            let mut s = self.state.write().await;
            s.set_active_skill(&skill);
            s.record_phase_read(&phase_file);
        }

        // Look up phase config for judge model + objectives
        let workflow_configs = load_workflow_configs();
        let (judge_model, phase_objectives) = workflow_configs
            .get(&skill)
            .and_then(|wf| wf.phases.iter().find(|p| p.id == phase_id))
            .map(|phase| {
                let desc = if phase.description.is_empty() {
                    format!("Complete the {phase_id} phase")
                } else {
                    phase.description.clone()
                };
                (phase.judge, desc)
            })
            .unwrap_or((JudgeModel::Sonnet, format!("Complete the {phase_id} phase")));

        // Build evidence from summary + state context
        let evidence = {
            let s = self.state.read().await;
            let mut ev = Evidence::default();
            ev.phase_file_read = true;
            ev.custom = serde_json::json!({
                "summary": summary,
                "phases_read": s.phases_read,
                "tool_calls_in_session": s.tool_calls,
                "hook_invocations": s.hook_stats.total_invocations,
                "blocked_count": s.hook_stats.total_blocked,
                "completed_phases": s.workflows.get(&skill).map(|w| &w.completed_phases),
                "active_skill": s.active_skill,
            });

            if let Some(wf) = s.workflows.get(&skill) {
                let step_states = wf.phase_step_states(&phase_id);
                for ss in &step_states {
                    match ss.status {
                        StepStatus::Completed => ev.steps_completed.push(ss.step_id.clone()),
                        StepStatus::Skipped => ev.steps_skipped.push(ss.step_id.clone()),
                        _ => {}
                    }
                }
            }
            ev
        };

        // Generate cryptographic proof
        let started_at = Utc::now() - chrono::Duration::seconds(1);
        let proof_result = self
            .proof_engine
            .submit_evidence(&skill, &phase_id, &phase_objectives, evidence, judge_model, started_at)
            .await;

        // Persist state to disk regardless of outcome
        {
            let s = self.state.read().await;
            if let Err(e) = sentinel_infrastructure::state_store::save(&s) {
                tracing::error!(error = %e, "Failed to persist session state — proof chain may be lost on crash");
            }
        }

        // INVISIBLE JUDGING: Claude never sees the verdict reasoning.
        // On success: minimal info with tessera hash (crypto proof).
        // On failure: opaque BLOCKED error — reasoning sealed in proof store.
        match proof_result {
            Ok(_) => {
                let (completed, tessera) = {
                    let s = self.state.read().await;
                    let completed = s
                        .workflows
                        .get(&skill)
                        .map(|w| w.completed_phases.clone())
                        .unwrap_or_default();
                    let tessera = s
                        .proof_chains
                        .get(&skill)
                        .and_then(|chain| chain.proofs.last())
                        .map(|p| p.combined_hash[..12].to_string())
                        .unwrap_or_default();
                    (completed, tessera)
                };

                let result = serde_json::json!({
                    "phase_id": phase_id,
                    "status": "accepted",
                    "tessera": tessera,
                    "completed_phases": completed,
                });

                serde_json::to_string_pretty(&result)
                    .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None))
            }
            Err(_) => {
                // BLOCKED — Claude sees generic error, NOT the judge's reasoning.
                // The actual verdict is stored in the proof chain / proof store.
                Err(ErrorData::internal_error(
                    format!(
                        "Phase '{}' BLOCKED — evidence insufficient. \
                         Re-run the phase with complete outputs before re-submitting.",
                        phase_id
                    ),
                    None,
                ))
            }
        }
    }

    /// Get execution statistics for the current session.
    /// Returns hook invocations, blocked calls, and per-hook timing.
    #[tool(description = "Get execution statistics for the current session — hook invocations, blocked calls, per-hook timing.")]
    async fn get_session_stats(&self) -> Result<String, ErrorData> {
        let s = self.state.read().await;
        let stats = serde_json::json!({
            "session_id": s.session_id,
            "active_skill": s.active_skill,
            "total_invocations": s.hook_stats.total_invocations,
            "total_blocked": s.hook_stats.total_blocked,
            "per_hook": s.hook_stats.per_hook,
            "workflows": s.workflows.keys().collect::<Vec<_>>(),
            "proof_chains": s.proof_chains.keys().collect::<Vec<_>>(),
        });

        serde_json::to_string_pretty(&stats)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None))
    }

    /// Update a step's status within a skill phase.
    /// Call this as you complete each step in a workflow phase.
    #[tool(description = "Update a step's status within a skill phase. Call this as you complete each step in a workflow phase.")]
    async fn update_step(
        &self,
        skill: String,
        phase_id: String,
        step_id: String,
        status: String,
        summary: Option<String>,
    ) -> Result<String, ErrorData> {
        // Parse status
        let step_status: StepStatus = serde_json::from_value(serde_json::json!(status))
            .map_err(|_| {
                ErrorData::invalid_params(
                    format!("Invalid status '{status}'. Use: completed, skipped, blocked, in_progress"),
                    None,
                )
            })?;

        let mut s = self.state.write().await;
        s.set_active_skill(&skill);

        if let Some(wf) = s.workflows.get_mut(&skill) {
            wf.update_step(&phase_id, &step_id, step_status, summary);
        }

        // Compute progress
        let phase_completed = s
            .workflows
            .get(&skill)
            .map_or(0, |w| w.phase_steps_completed(&phase_id));

        let steps_config = load_steps_config(&skill);
        let phase_total = steps_config
            .as_ref()
            .and_then(|sc| sc.phase_steps(&phase_id))
            .map(|ps| ps.steps.len())
            .unwrap_or_else(|| {
                s.workflows
                    .get(&skill)
                    .map_or(0, |w| w.phase_step_states(&phase_id).len())
            });

        let overall_progress = steps_config.as_ref().map(|sc| {
            let total = sc.total_steps();
            let completed = s
                .workflows
                .get(&skill)
                .map_or(0, sentinel_domain::workflow::WorkflowState::total_steps_completed);
            format!("{completed}/{total} steps")
        });

        let _ = sentinel_infrastructure::state_store::save(&s);

        let mut result = serde_json::json!({
            "step_id": step_id,
            "phase_id": phase_id,
            "status": status,
            "phase_progress": format!("{phase_completed}/{phase_total} steps"),
        });

        if let Some(overall) = overall_progress {
            result
                .as_object_mut()
                .unwrap()
                .insert("overall_progress".to_string(), serde_json::json!(overall));
        }

        serde_json::to_string_pretty(&result)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None))
    }

    /// Get all steps and their status for a specific phase.
    /// Shows step descriptions from config and current execution status.
    #[tool(description = "Get all steps and their status for a specific phase. Shows step descriptions from config and current execution status.")]
    async fn get_phase_steps(
        &self,
        skill: String,
        phase_id: String,
    ) -> Result<String, ErrorData> {
        let s = self.state.read().await;
        let steps_config = load_steps_config(&skill);

        let mut steps_list: Vec<serde_json::Value> = Vec::new();

        if let Some(ref sc) = steps_config {
            if let Some(phase_steps) = sc.phase_steps(&phase_id) {
                for step_def in &phase_steps.steps {
                    let step_state = s
                        .workflows
                        .get(&skill)
                        .and_then(|wf| {
                            wf.step_states
                                .iter()
                                .find(|ss| ss.step_id == step_def.id && ss.phase_id == phase_id)
                        });

                    let status = step_state
                        .map_or(StepStatus::Pending, |ss| ss.status.clone());
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
        } else if let Some(wf) = s.workflows.get(&skill) {
            for ss in wf.phase_step_states(&phase_id) {
                let mut entry = serde_json::json!({
                    "id": ss.step_id,
                    "description": null,
                    "status": ss.status,
                });
                if let Some(ref sum) = ss.summary {
                    entry
                        .as_object_mut()
                        .unwrap()
                        .insert("summary".to_string(), serde_json::json!(sum));
                }
                steps_list.push(entry);
            }
        }

        let completed = s
            .workflows
            .get(&skill)
            .map_or(0, |w| w.phase_steps_completed(&phase_id));
        let total = steps_list.len();

        let result = serde_json::json!({
            "phase_id": phase_id,
            "steps": steps_list,
            "completed": completed,
            "total": total,
        });

        serde_json::to_string_pretty(&result)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None))
    }

    /// Get full hierarchical progress for a skill workflow.
    /// Shows phase-level and step-level completion across the entire workflow.
    #[tool(description = "Get full hierarchical progress for a skill workflow. Shows phase-level and step-level completion across the entire workflow.")]
    async fn get_workflow_progress(&self, skill: String) -> Result<String, ErrorData> {
        let s = self.state.read().await;
        let steps_config = load_steps_config(&skill);
        let workflow_configs = load_workflow_configs();

        let wf_state: Option<&WorkflowState> = s.workflows.get(&skill);

        let mut phases_list: Vec<serde_json::Value> = Vec::new();
        let mut overall_completed: usize = 0;
        let mut overall_total: usize = 0;

        if let Some(workflow) = workflow_configs.get(&skill) {
            for phase in &workflow.phases {
                let phase_status = if wf_state
                    .is_some_and(|w| w.is_phase_complete(&phase.id))
                {
                    "completed"
                } else if wf_state
                    .is_some_and(|w| {
                        w.current_phase.is_some()
                            && !w.completed_phases.contains(&phase.id)
                            && w.completed_phases.len()
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

                let steps_completed = wf_state
                    .map_or(0, |w| w.phase_steps_completed(&phase.id));

                let steps_total = steps_config
                    .as_ref()
                    .and_then(|sc| sc.phase_steps(&phase.id))
                    .map(|ps| ps.steps.len())
                    .unwrap_or_else(|| {
                        wf_state
                            .map_or(0, |w| w.phase_step_states(&phase.id).len())
                    });

                overall_completed += steps_completed;
                overall_total += steps_total;

                let mut step_details: Vec<serde_json::Value> = Vec::new();
                if let Some(ref sc) = steps_config {
                    if let Some(phase_steps) = sc.phase_steps(&phase.id) {
                        for step_def in &phase_steps.steps {
                            let step_state = wf_state.and_then(|wf| {
                                wf.step_states
                                    .iter()
                                    .find(|ss| ss.step_id == step_def.id && ss.phase_id == phase.id)
                            });

                            let st = step_state
                                .map_or(StepStatus::Pending, |ss| ss.status.clone());

                            step_details.push(serde_json::json!({
                                "id": step_def.id,
                                "description": step_def.description,
                                "status": st,
                            }));
                        }
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
        } else if let Some(wf) = wf_state {
            // No workflow config — report from runtime state
            let mut phase_map: HashMap<String, Vec<&sentinel_domain::workflow::StepState>> =
                HashMap::new();
            for ss in &wf.step_states {
                phase_map.entry(ss.phase_id.clone()).or_default().push(ss);
            }

            for (pid, states) in &phase_map {
                let completed = states
                    .iter()
                    .filter(|s| {
                        s.status == StepStatus::Completed || s.status == StepStatus::Skipped
                    })
                    .count();
                let total = states.len();
                overall_completed += completed;
                overall_total += total;

                phases_list.push(serde_json::json!({
                    "id": pid,
                    "description": null,
                    "status": if wf.is_phase_complete(pid) { "completed" } else { "in_progress" },
                    "steps_completed": completed,
                    "steps_total": total,
                }));
            }
        }

        let percentage = if overall_total > 0 {
            (overall_completed as f64 / overall_total as f64 * 100.0).round() as u32
        } else {
            0
        };

        let result = serde_json::json!({
            "skill": skill,
            "phases": phases_list,
            "overall": {
                "steps_completed": overall_completed,
                "steps_total": overall_total,
                "percentage": percentage,
            }
        });

        serde_json::to_string_pretty(&result)
            .map_err(|e| ErrorData::internal_error(format!("Serialization error: {e}"), None))
    }
}

// ============================================================================
// ServerHandler implementation
// ============================================================================

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SentinelMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .build(),
            server_info: Implementation {
                name: "sentinel-mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(env!("CARGO_PKG_DESCRIPTION").to_string()),
                ..Default::default()
            },
            instructions: Some(
                "Sentinel proof-of-work hook engine. Query proof chains, workflow state, \
                 session stats, and step-level progress for skill executions."
                    .to_string(),
            ),
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn load_steps_config(skill: &str) -> Option<SkillSteps> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    sentinel_infrastructure::config::load_skill_steps(&config_dir, skill)
        .ok()
        .flatten()
}

fn load_workflow_configs() -> HashMap<String, sentinel_domain::workflow::SkillWorkflow> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    if config_dir.join("workflows.toml").exists() {
        sentinel_infrastructure::config::load_workflows(&config_dir)
            .unwrap_or_default()
            .into_iter()
            .map(|w| (w.skill.clone(), w))
            .collect()
    } else {
        HashMap::new()
    }
}

// ============================================================================
// Entry point
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    info!("Starting Sentinel MCP server (Vulcan SDK)");

    let server = SentinelMcp::new();
    let service = server.serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Sentinel MCP serving error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}
