//! `sentinel mcp` — MCP server over stdio
//!
//! Claude Code connects to this as an MCP server.
//! Reads JSON-RPC requests from stdin, writes responses to stdout.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use sentinel_application::judge_service::FallbackJudge;
use sentinel_application::mcp_handler::{McpHandler, McpToolCall};
use sentinel_application::proof_engine::ProofEngine;
use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::JudgeModel;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, StepStatus, WorkflowState};
use sentinel_infrastructure::mcp_transport::{JsonRpcRequest, JsonRpcResponse};

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
                        }
                    },
                    "required": ["skill", "phase_id", "summary"]
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
                "description": "Update a step's status within a skill phase. Call this as you complete each step in a workflow phase.",
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
                            "enum": ["completed", "skipped", "blocked", "in_progress"],
                            "description": "New status for the step"
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
            }
        ]
    })
}

/// Server info for MCP initialize response
fn server_info() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "sentinel",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

pub async fn run() -> Result<()> {
    // Use real session ID from environment, falling back to timestamped ID
    let session_id = std::env::var("SESSION_ID")
        .or_else(|_| std::env::var("CLAUDE_SESSION_ID"))
        .unwrap_or_else(|_| format!("mcp-{}", Utc::now().timestamp()));

    // Try loading existing state from disk (so MCP and hooks share state)
    let state = match sentinel_infrastructure::state_store::load(&session_id) {
        Ok(Some(existing)) => {
            info!(session_id = %session_id, "Loaded existing session state from disk");
            Arc::new(RwLock::new(existing))
        }
        _ => {
            info!(session_id = %session_id, "Creating new session state");
            Arc::new(RwLock::new(SessionState::new(&session_id)))
        }
    };

    let judge: Arc<dyn sentinel_application::judge_service::JudgeService> =
        match sentinel_infrastructure::anthropic::AnthropicClient::from_env() {
            Ok(client) => Arc::new(client),
            Err(_) => {
                info!("No ANTHROPIC_API_KEY — using fallback judge");
                Arc::new(FallbackJudge)
            }
        };
    let proof_engine = Arc::new(ProofEngine::new(state.clone(), judge.clone()));
    let handler = McpHandler::new(state.clone(), proof_engine.clone());

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

        let response = handle_request(&request, &handler, &state, &proof_engine).await;

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

        "initialized" => {
            // Notification — no response needed, but we send one anyway for safety
            JsonRpcResponse::success(request.id.clone(), serde_json::json!({}))
        }

        // Tool listing
        "tools/list" => JsonRpcResponse::success(request.id.clone(), tool_definitions()),

        // Tool execution
        "tools/call" => {
            let tool_name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_default();

            // Handle submit_phase_complete specially (needs state mutation + proof generation)
            if tool_name == "sentinel__submit_phase_complete" {
                return handle_submit_phase(request, &arguments, state, proof_engine).await;
            }

            // Handle step tracking tools specially (need state mutation)
            if tool_name == "sentinel__update_step" {
                return handle_update_step(request, &arguments, state).await;
            }
            if tool_name == "sentinel__get_phase_steps" {
                return handle_get_phase_steps(request, &arguments, state).await;
            }
            if tool_name == "sentinel__get_workflow_progress" {
                return handle_get_workflow_progress(request, &arguments, state).await;
            }

            // Handle get_session_stats specially
            if tool_name == "sentinel__get_session_stats" {
                let s = state.read().await;
                let stats = serde_json::json!({
                    "session_id": s.session_id,
                    "active_skill": s.active_skill,
                    "total_invocations": s.hook_stats.total_invocations,
                    "total_blocked": s.hook_stats.total_blocked,
                    "per_hook": s.hook_stats.per_hook,
                    "workflows": s.workflows.keys().collect::<Vec<_>>(),
                    "proof_chains": s.proof_chains.keys().collect::<Vec<_>>(),
                });
                return JsonRpcResponse::success(
                    request.id.clone(),
                    mcp_tool_result(true, stats),
                );
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
                    mcp_tool_result(false, serde_json::json!({"error": result.error})),
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

/// Handle sentinel__submit_phase_complete
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
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'skill'"})),
            )
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'phase_id'"})),
            )
        }
    };
    let summary = args
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Record phase read in state (submitting implies the phase was read)
    let phase_file = format!("{}.md", phase_id);
    {
        let mut s = state.write().await;
        s.set_active_skill(&skill);
        s.record_phase_read(&phase_file);
    }

    // Look up phase config for judge model + objectives from workflows.toml
    let workflow_configs = load_workflow_configs();
    let (judge_model, phase_objectives) = workflow_configs
        .get(&skill)
        .and_then(|wf| wf.phases.iter().find(|p| p.id == phase_id))
        .map(|phase| {
            let desc = if phase.description.is_empty() {
                format!("Complete the {} phase", phase_id)
            } else {
                phase.description.clone()
            };
            (phase.judge, desc)
        })
        .unwrap_or((
            JudgeModel::Sonnet,
            format!("Complete the {} phase", phase_id),
        ));

    // Build evidence from the summary + state context
    let evidence = {
        let s = state.read().await;
        let mut ev = Evidence::default();
        ev.phase_file_read = true;
        ev.custom = serde_json::json!({
            "summary": summary,
            "phases_read": s.phases_read,
            "tool_calls_in_session": s.tool_calls,
        });

        // Include step evidence if steps were tracked for this phase
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

    // Generate cryptographic proof via the proof engine
    let started_at = Utc::now() - chrono::Duration::seconds(1); // Approximate phase start
    let proof_result = proof_engine
        .submit_evidence(&skill, &phase_id, &phase_objectives, evidence, judge_model, started_at)
        .await;

    // Get completed phases and proof info
    let (completed, proof_info) = {
        let s = state.read().await;
        let completed = s
            .workflows
            .get(&skill)
            .map(|w| w.completed_phases.clone())
            .unwrap_or_default();
        let proof_info = s
            .proof_chains
            .get(&skill)
            .and_then(|chain| chain.proofs.last())
            .map(|p| {
                serde_json::json!({
                    "tessera_hash": &p.combined_hash[..12],
                    "evidence_hash": &p.evidence_hash[..12],
                    "judge_model": p.judge_model,
                    "judge_sufficient": p.judge_verdict.sufficient,
                    "judge_confidence": p.judge_verdict.confidence,
                })
            });
        (completed, proof_info)
    };

    // Persist state to disk (so hooks can see the proof chain)
    {
        let s = state.read().await;
        let _ = sentinel_infrastructure::state_store::save(&s);
    }

    let mut result = serde_json::json!({
        "phase_id": phase_id,
        "skill": skill,
        "summary": summary,
        "completed_phases": completed,
        "message": format!("Phase '{}' proven and recorded", phase_id),
    });

    match proof_result {
        Ok(_) => {
            if let Some(pi) = proof_info {
                result
                    .as_object_mut()
                    .unwrap()
                    .insert("proof".to_string(), pi);
            }
        }
        Err(e) => {
            warn!(phase = %phase_id, error = %e, "Proof generation failed — phase still tracked");
            result.as_object_mut().unwrap().insert(
                "proof_error".to_string(),
                serde_json::json!(e.to_string()),
            );
        }
    }

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Helper: Load skill steps config from the config directory
fn load_steps_config(skill: &str) -> Option<SkillSteps> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    sentinel_infrastructure::config::load_skill_steps(&config_dir, skill)
        .ok()
        .flatten()
}

/// Helper: Load workflow configs as a HashMap
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

/// Handle sentinel__update_step
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
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'skill'"})),
            )
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'phase_id'"})),
            )
        }
    };
    let step_id = match args.get("step_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'step_id'"})),
            )
        }
    };
    let status_str = match args.get("status").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'status'"})),
            )
        }
    };
    let summary = args
        .get("summary")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Parse status
    let status: StepStatus = match serde_json::from_value(serde_json::json!(status_str)) {
        Ok(s) => s,
        Err(_) => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(
                    false,
                    serde_json::json!({"error": format!("Invalid status '{}'. Use: completed, skipped, blocked, in_progress", status_str)}),
                ),
            )
        }
    };

    // Update state
    let mut s = state.write().await;
    s.set_active_skill(&skill);

    if let Some(wf) = s.workflows.get_mut(&skill) {
        wf.update_step(&phase_id, &step_id, status.clone(), summary);
    }

    // Compute progress
    let phase_completed = s
        .workflows
        .get(&skill)
        .map(|w| w.phase_steps_completed(&phase_id))
        .unwrap_or(0);

    // Phase total: from config if available, else from tracked states
    let steps_config = load_steps_config(&skill);
    let phase_total = steps_config
        .as_ref()
        .and_then(|sc| sc.phase_steps(&phase_id))
        .map(|ps| ps.steps.len())
        .unwrap_or_else(|| {
            s.workflows
                .get(&skill)
                .map(|w| w.phase_step_states(&phase_id).len())
                .unwrap_or(0)
        });

    let phase_progress = format!("{}/{} steps", phase_completed, phase_total);

    // Overall progress (only if steps config exists)
    let overall_progress = steps_config.as_ref().map(|sc| {
        let total = sc.total_steps();
        let completed = s
            .workflows
            .get(&skill)
            .map(|w| w.total_steps_completed())
            .unwrap_or(0);
        format!("{}/{} steps", completed, total)
    });

    // Save state to disk
    let _ = sentinel_infrastructure::state_store::save(&s);

    let mut result = serde_json::json!({
        "step_id": step_id,
        "phase_id": phase_id,
        "status": status_str,
        "phase_progress": phase_progress,
    });

    if let Some(overall) = overall_progress {
        result
            .as_object_mut()
            .unwrap()
            .insert("overall_progress".to_string(), serde_json::json!(overall));
    }

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Handle sentinel__get_phase_steps
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
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'skill'"})),
            )
        }
    };
    let phase_id = match args.get("phase_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'phase_id'"})),
            )
        }
    };

    let s = state.read().await;
    let steps_config = load_steps_config(&skill);

    // Build step list — merge config definitions with runtime state
    let mut steps_list: Vec<serde_json::Value> = Vec::new();

    if let Some(ref sc) = steps_config {
        if let Some(phase_steps) = sc.phase_steps(&phase_id) {
            for step_def in &phase_steps.steps {
                // Find runtime state for this step
                let step_state = s
                    .workflows
                    .get(&skill)
                    .and_then(|wf| {
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
    } else {
        // No config — just report tracked states
        if let Some(wf) = s.workflows.get(&skill) {
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
    }

    let completed = s
        .workflows
        .get(&skill)
        .map(|w| w.phase_steps_completed(&phase_id))
        .unwrap_or(0);
    let total = steps_list.len();

    let result = serde_json::json!({
        "phase_id": phase_id,
        "steps": steps_list,
        "completed": completed,
        "total": total,
    });

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Handle sentinel__get_workflow_progress
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
                mcp_tool_result(false, serde_json::json!({"error": "Missing 'skill'"})),
            )
        }
    };

    let s = state.read().await;
    let steps_config = load_steps_config(&skill);
    let workflow_configs = load_workflow_configs();

    let wf_state: Option<&WorkflowState> = s.workflows.get(&skill);

    // Build phase-level progress
    let mut phases_list: Vec<serde_json::Value> = Vec::new();
    let mut overall_completed: usize = 0;
    let mut overall_total: usize = 0;

    if let Some(workflow) = workflow_configs.get(&skill) {
        for phase in &workflow.phases {
            let phase_status = if wf_state
                .map(|w| w.is_phase_complete(&phase.id))
                .unwrap_or(false)
            {
                "completed"
            } else if wf_state
                .map(|w| w.current_phase.is_some() && !w.completed_phases.contains(&phase.id))
                .unwrap_or(false)
                && wf_state
                    .map(|w| {
                        w.completed_phases.len()
                            == workflow
                                .phases
                                .iter()
                                .position(|p| p.id == phase.id)
                                .unwrap_or(0)
                    })
                    .unwrap_or(false)
            {
                "in_progress"
            } else if wf_state
                .map(|w| w.is_phase_complete(&phase.id))
                .unwrap_or(false)
            {
                "completed"
            } else {
                "pending"
            };

            // Step-level counts for this phase
            let steps_completed = wf_state
                .map(|w| w.phase_steps_completed(&phase.id))
                .unwrap_or(0);

            let steps_total = steps_config
                .as_ref()
                .and_then(|sc| sc.phase_steps(&phase.id))
                .map(|ps| ps.steps.len())
                .unwrap_or_else(|| {
                    wf_state
                        .map(|w| w.phase_step_states(&phase.id).len())
                        .unwrap_or(0)
                });

            overall_completed += steps_completed;
            overall_total += steps_total;

            // Build step details for this phase
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
    } else {
        // No workflow config — report what we have from runtime state
        if let Some(wf) = wf_state {
            // Group step states by phase
            let mut phase_map: HashMap<String, Vec<&sentinel_domain::workflow::StepState>> =
                HashMap::new();
            for ss in &wf.step_states {
                phase_map
                    .entry(ss.phase_id.clone())
                    .or_default()
                    .push(ss);
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

    JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
}

/// Format MCP tool result in the standard content array format
fn mcp_tool_result(success: bool, data: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&data).unwrap_or_default()
        }],
        "isError": !success
    })
}
