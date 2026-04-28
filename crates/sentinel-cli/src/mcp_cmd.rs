//! `sentinel mcp` — MCP server over stdio
//!
//! Claude Code connects to this as an MCP server.
//! Reads JSON-RPC requests from stdin, writes responses to stdout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result};
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
/// since the tool call just happened). If not found there, fall back to
/// scanning all transcripts.
///
/// Returns `None` if no transcript contains the id — treat that as "fall
/// back to newest-mtime" (the id may be too fresh for the transcript
/// writer to have flushed, though this race is vanishingly rare since
/// Claude Code flushes after each message).
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
    transcripts.sort_by(|a, b| b.0.cmp(&a.0)); // newest first

    for (_, path, session_id) in transcripts {
        if transcript_contains_tool_use_id(&path, tool_use_id) {
            return Some(session_id);
        }
    }
    None
}

/// Scan a single transcript JSONL for an assistant `tool_use` whose `id`
/// matches the given `tool_use_id`. Reads the file fully into memory and
/// walks lines backwards — `tool_use` ids are overwhelmingly at the tail.
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
/// Preference order (highest confidence first):
///   1. `params._meta["claudecode/toolUseId"]` → cross-reference against
///      transcript JSONLs. Disambiguates when multiple Claude Code
///      windows are open; this is the only fully reliable signal.
///   2. Newest-mtime transcript under `~/.claude/projects/`. Used for
///      requests without a toolUseId (e.g. `initialize`, `ping`, internal
///      calls) and as a safety fallback if the toolUseId lookup misses
///      (e.g. due to transcript-flush timing).
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
        // toolUseId present but not yet in any transcript — fall through to
        // newest-mtime heuristic. Logged so the race is visible.
        debug!(
            tool_use_id,
            "toolUseId not found in any transcript; falling back to newest-mtime"
        );
    }

    // 2. Fallback: newest-mtime transcript.
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
    let loaded = match sentinel_infrastructure::state_store::load(session_id) {
        Ok(Some(s)) => s,
        Ok(None) => SessionState::new(session_id),
        Err(e) => {
            return Err(e).context("state_store::load failed");
        }
    };

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
    // cross-reference → newest-mtime fallback) and the `with_session_state`
    // transaction handles lock/load/save. The Arc<RwLock<SessionState>>
    // here is a placeholder whose contents are overwritten on every call.
    // See the header comment above `detect_live_session_id` for rationale.
    let state = Arc::new(RwLock::new(SessionState::new("uninitialized")));

    let judge: Arc<dyn sentinel_application::judge_service::JudgeService> = {
        let multi = sentinel_infrastructure::rig_judge::MultiModelJudge::from_env();
        if multi.has_any_provider() {
            Arc::new(multi)
        } else {
            warn!("No AI judge providers available — using blocking fallback");
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
                    match with_session_state(&session_id, state_ref, move || async move {
                        handle_request(req_ref, handler_ref, state_ref, proof_ref).await
                    })
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
            let tool_name = request
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = request.params.get("arguments").cloned().unwrap_or_default();

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

            // CLAUDE.md management — shared implementation with the CLI
            // subcommands lives in `crate::claude_md_cmd`.
            if tool_name == "sentinel__regenerate_claude_md" {
                return match crate::claude_md_cmd::regenerate() {
                    Ok(v) => JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, v)),
                    Err(e) => JsonRpcResponse::success(
                        request.id.clone(),
                        mcp_tool_result(false, serde_json::json!({"error": e.to_string()})),
                    ),
                };
            }
            if tool_name == "sentinel__edit_claude_md_template" {
                let find = arguments.get("find").and_then(|v| v.as_str()).unwrap_or("");
                let replace = arguments
                    .get("replace")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                return match crate::claude_md_cmd::edit_template(find, replace) {
                    Ok(v) => JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, v)),
                    Err(e) => JsonRpcResponse::success(
                        request.id.clone(),
                        mcp_tool_result(false, serde_json::json!({"error": e.to_string()})),
                    ),
                };
            }
            if tool_name == "sentinel__restart_all_mcps" {
                return match crate::claude_md_cmd::restart_all_mcps() {
                    Ok(v) => JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, v)),
                    Err(e) => JsonRpcResponse::success(
                        request.id.clone(),
                        mcp_tool_result(false, serde_json::json!({"error": e.to_string()})),
                    ),
                };
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

    // Look up phase config for judge model + objectives from workflows.toml
    let workflow_configs = load_workflow_configs();

    // **Attack #142 fix**: Verify the skill has a workflow definition before
    // recording phase reads. Without this check, an attacker could submit evidence
    // for a non-existent skill, creating workflow state entries that have no
    // enforcement gates (no phases to complete, so everything passes).
    if !workflow_configs.contains_key(&skill) {
        return JsonRpcResponse::success(
            request.id.clone(),
            mcp_tool_result(
                false,
                serde_json::json!({"error": format!(
                    "No workflow definition for skill '{}'. Cannot submit evidence.", skill
                )}),
            ),
        );
    }

    // Record phase read in state (submitting implies the phase was read)
    let phase_file = format!("{phase_id}.md");
    {
        let mut s = state.write().await;
        s.set_active_skill(&skill);
        s.record_phase_read(&skill, &phase_file);
    }
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

    // Build evidence from the summary + state context
    let evidence = {
        let s = state.read().await;
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
        .submit_evidence(
            &skill,
            &phase_id,
            &phase_objectives,
            evidence,
            judge_model,
            started_at,
            workflow_configs.get(&skill),
        )
        .await;

    // Get completed phases and tessera (hash only — verdict details stay sealed)
    let (completed, tessera) = {
        let s = state.read().await;
        let completed = s
            .workflows
            .get(&skill)
            .map(|w| w.completed_phases.clone())
            .unwrap_or_default();
        let tessera = s
            .proof_chains
            .get(&skill)
            .and_then(|chain| chain.proofs.last())
            .map(|p| p.combined_hash[..12].to_string());
        (completed, tessera)
    };

    // Persist state to disk (so hooks can see the proof chain)
    {
        let mut s = state.write().await;
        if let Err(e) = sentinel_infrastructure::state_store::save(&mut s) {
            warn!(error = %e, "Failed to persist session state — proof chain may be lost on crash");
        }
    }

    match proof_result {
        Ok(_) => {
            // SUCCESS — minimal info, no judge reasoning exposed
            let result = serde_json::json!({
                "phase_id": phase_id,
                "status": "accepted",
                "tessera": tessera.unwrap_or_default(),
                "completed_phases": completed,
            });
            JsonRpcResponse::success(request.id.clone(), mcp_tool_result(true, result))
        }
        Err(e) => {
            // BLOCKED — opaque error, reasoning sealed in proof store
            warn!(phase = %phase_id, error = %e, "Phase BLOCKED by AI judge");
            let error_msg = format!(
                "Phase '{phase_id}' BLOCKED — evidence insufficient. Re-run the phase with complete outputs before re-submitting."
            );
            JsonRpcResponse::success(
                request.id.clone(),
                mcp_tool_result(false, serde_json::json!({"error": error_msg})),
            )
        }
    }
}

/// Helper: Load skill steps config from the config directory
fn load_steps_config(skill: &str) -> Option<SkillSteps> {
    let config_dir = sentinel_infrastructure::config::config_dir();
    sentinel_infrastructure::config::load_skill_steps(&config_dir, skill)
        .ok()
        .flatten()
}

/// Helper: Load workflow configs as a `HashMap`
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
        .map(std::string::ToString::to_string);

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
        .map_or(0, |w| w.phase_steps_completed(&phase_id));

    // Phase total: from config if available, else from tracked states
    let steps_config = load_steps_config(&skill);
    let phase_total = steps_config
        .as_ref()
        .and_then(|sc| sc.phase_steps(&phase_id))
        .map_or_else(
            || {
                s.workflows
                    .get(&skill)
                    .map_or(0, |w| w.phase_step_states(&phase_id).len())
            },
            |ps| ps.steps.len(),
        );

    let phase_progress = format!("{phase_completed}/{phase_total} steps");

    // Overall progress (only if steps config exists)
    let overall_progress = steps_config.as_ref().map(|sc| {
        let total = sc.total_steps();
        let completed = s.workflows.get(&skill).map_or(
            0,
            sentinel_domain::workflow::WorkflowState::total_steps_completed,
        );
        format!("{completed}/{total} steps")
    });

    // Save state to disk
    if let Err(e) = sentinel_infrastructure::state_store::save(&mut s) {
        tracing::warn!(error = %e, "Failed to persist state after step update");
    }

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
                let step_state = s.workflows.get(&skill).and_then(|wf| {
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
        .map_or(0, |w| w.phase_steps_completed(&phase_id));
    let total = steps_list.len();

    let result = serde_json::json!({
        "phase_id": phase_id,
        "steps": steps_list,
        "completed": completed,
        "total": total,
    });

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
            let phase_status = if wf_state.is_some_and(|w| w.is_phase_complete(&phase.id)) {
                "completed"
            } else if wf_state.is_some_and(|w| {
                w.current_phase.is_some() && !w.completed_phases.contains(&phase.id)
            }) && wf_state.is_some_and(|w| {
                w.completed_phases.len()
                    == workflow
                        .phases
                        .iter()
                        .position(|p| p.id == phase.id)
                        .unwrap_or(0)
            }) {
                "in_progress"
            } else {
                "pending"
            };

            // Step-level counts for this phase
            let steps_completed = wf_state.map_or(0, |w| w.phase_steps_completed(&phase.id));

            let steps_total = steps_config
                .as_ref()
                .and_then(|sc| sc.phase_steps(&phase.id))
                .map_or_else(
                    || wf_state.map_or(0, |w| w.phase_step_states(&phase.id).len()),
                    |ps| ps.steps.len(),
                );

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Write a UUID-shaped stem for a given suffix char to make fixture
    /// session ids easy to tell apart.
    fn uuid_like(suffix: char) -> String {
        format!("11111111-2222-3333-4444-55555555555{suffix}")
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
    fn resolve_session_id_falls_back_to_newest_when_tool_use_id_unknown() {
        // toolUseId supplied but not in any transcript (race: model called
        // tool, transcript writer hasn't flushed yet). Must fall back to
        // newest-mtime rather than error out.
        let lock = ENV_LOCK.lock().unwrap();
        with_fake_home(|home| {
            let id = uuid_like('a');
            seed_transcript(home, "p", &id, &[serde_json::json!({"type": "user"})]);

            let params = serde_json::json!({
                "_meta": {"claudecode/toolUseId": "toolu_never_recorded"}
            });
            let resolved = resolve_session_id(&params).unwrap();
            assert_eq!(
                resolved, id,
                "must fall back to newest-mtime on missing toolUseId"
            );
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
        let handle = Arc::new(RwLock::new(SessionState::new("placeholder")));

        // First transaction: set active_skill.
        let handle1 = handle.clone();
        with_session_state(&session_id, &handle, move || {
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
        with_session_state(&session_id, &handle, move || {
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

    /// Tests mutate process env (`HOME/USERPROFILE/SENTINEL_STATE_DIR`) and
    /// must serialize — cargo test runs in parallel by default.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
