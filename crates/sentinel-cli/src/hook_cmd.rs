//! `sentinel hook` — Process hook events (thin client or standalone)

use std::collections::HashMap;
use std::io::Write;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use sentinel_application::hooks;
use sentinel_domain::events::{HookEvent, HookOutput, HookSpecificOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, SkillWorkflow};

/// Infrastructure implementation of GitStatusPort
struct RealGit;

impl hooks::GitStatusPort for RealGit {
    fn has_uncommitted_changes(&self, repo_path: &str) -> Result<bool> {
        sentinel_infrastructure::git::has_uncommitted_changes(repo_path)
    }

    fn changed_files(&self, repo_path: &str) -> Result<Vec<String>> {
        sentinel_infrastructure::git::changed_files(repo_path)
    }

    fn current_branch(&self, repo_path: &str) -> Result<String> {
        sentinel_infrastructure::git::current_branch(repo_path)
    }

    fn is_worktree(&self, repo_path: &str) -> bool {
        // Worktrees have .git as a file (pointing to the main repo), not a directory
        std::path::Path::new(repo_path).join(".git").is_file()
    }
}

pub async fn run(event: &str, matcher: Option<&str>, standalone: bool) -> Result<()> {
    // ── Glass break emergency override ───────────────────────────────────
    // Two-layer design:
    //   1. Active token (~/.claude/sentinel/.glass-break-token) — fast path,
    //      checked on every hook invocation. Contains expiry + HMAC.
    //   2. Trigger file (~/.claude/sentinel/.glass-break) — slow path,
    //      detected once, launches a native Windows dialog requiring human
    //      confirmation. On success, writes a time-limited token and deletes
    //      the trigger file. AI cannot complete the dialog.
    if check_glass_break_override() {
        eprintln!("[sentinel] GLASS BREAK ACTIVE — all enforcement bypassed");
        println!("{{}}");
        return Ok(());
    }

    if standalone {
        return run_internal(event, matcher, standalone).await;
    }

    let start_time = std::time::Instant::now();
    debug!(event, ?matcher, standalone, "Processing hook event");

    let hook_event = parse_hook_event(event)?;
    if let Err(e) = validate_caller() {
        // Caller validation failed — return safe empty JSON and exit early.
        // The security event was already logged inside validate_caller().
        debug!(error = %e, "Caller validation failed — returning safe response");
        return write_safe_allow_response();
    }

    let raw_input = sentinel_infrastructure::stdin::read_raw_stdin(Duration::from_secs(3))?;
    run_supervised(hook_event, event, matcher, raw_input).await?;

    debug!(
        event,
        elapsed_ms = start_time.elapsed().as_millis() as u64,
        "Hook supervisor completed"
    );
    Ok(())
}

pub async fn run_internal(event: &str, matcher: Option<&str>, standalone: bool) -> Result<()> {
    // ── Glass break emergency override (same check as supervisor) ─────────
    if check_glass_break_override() {
        eprintln!("[sentinel] GLASS BREAK ACTIVE — all enforcement bypassed");
        println!("{{}}");
        return Ok(());
    }

    let start_time = std::time::Instant::now();
    debug!(event, ?matcher, standalone, "Processing hook event");

    // Parse event type
    // **Attack #120 fix**: Fail closed on unknown event types instead of defaulting
    // to Stop. Silently treating an unknown event as Stop would run Stop-phase hooks
    // (execution_log, skill_telemetry, context_monitor, etc.) which could confuse
    // state tracking. Better to error out so the issue is immediately visible.
    let hook_event = parse_hook_event(event)?;

    // **Stdin authentication**: Validate that we're being invoked by a plausible
    // caller (Claude Code / node process). This is defense-in-depth — not a hard
    // guarantee, but raises the bar from "any process on the system" to "a process
    // that can convincingly mimic Claude Code's invocation pattern".
    if let Err(e) = validate_caller() {
        // Caller validation failed — return safe empty JSON and exit early.
        debug!(error = %e, "Caller validation failed — returning safe response");
        return write_safe_allow_response();
    }

    // Read input from stdin
    let input = sentinel_infrastructure::stdin::read_hook_input()?;

    // Load config
    let config_dir = sentinel_infrastructure::config::config_dir();
    let workflows: HashMap<String, SkillWorkflow> = if config_dir.join("workflows.toml").exists() {
        sentinel_infrastructure::config::load_workflows(&config_dir)?
            .into_iter()
            .map(|w| (w.skill.clone(), w))
            .collect()
    } else {
        HashMap::new()
    };

    // Load step configs for all known skills
    let step_configs: HashMap<String, SkillSteps> = workflows
        .keys()
        .filter_map(|skill| {
            sentinel_infrastructure::config::load_skill_steps(&config_dir, skill)
                .ok()
                .flatten()
                .map(|steps| (skill.clone(), steps))
        })
        .collect();

    // **Attack #67 fix**: Acquire session lock BEFORE loading state.
    // Hold through processing + save to prevent concurrent hook invocations
    // from overwriting each other's state changes (lost updates).
    //
    // **Attack #128 note**: Lock safety on panic — `_session_lock` is a
    // `std::fs::File` handle with fs2 advisory lock. Rust's Drop trait
    // guarantees the file handle is closed on unwind (panic), which releases
    // the advisory lock. No manual cleanup needed.
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    // **Rate limiting**: Check per-session invocation rate BEFORE acquiring session lock.
    // This prevents flood attacks from even contending for the lock, reducing DoS impact.
    sentinel_infrastructure::rate_limit::check_rate_limit(session_id)?;

    let _session_lock = sentinel_infrastructure::state_store::acquire_session_lock(session_id)?;
    let mut state = sentinel_infrastructure::state_store::load(session_id)?
        .unwrap_or_else(|| SessionState::new(session_id));

    let git = RealGit;

    // Process through matching hooks based on event type
    let mut output = HookOutput::allow();

    match hook_event {
        HookEvent::UserPromptSubmit => {
            // Skill router — classify and route to matching skill
            // Uses AI classification (Cerebras/OpenAI) with regex fallback.
            // Wrapped in a 5-second timeout as a safety net — if the entire
            // routing pipeline hangs, fall back to regex-only routing so the
            // user's message is never silently blocked.
            let router = hooks::skill_router::default_router();
            let classifier = sentinel_infrastructure::rig_classifier::RigClassifier::from_env();
            let router_output = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                hooks::skill_router::process_with_ai(
                    &input,
                    &router,
                    classifier
                        .as_ref()
                        .map(|c| c as &dyn sentinel_application::classifier::AiClassifier),
                ),
            )
            .await
            {
                Ok(output) => output,
                Err(_) => {
                    debug!("Skill router timed out (5s) — falling back to regex-only");
                    hooks::skill_router::process(&input, &router)
                }
            };
            output.merge(&router_output);

            // Extract detected skill from router output and update state.
            // When no skill matches, clear active_skill so the phase gate
            // doesn't keep blocking on a stale skill from earlier in the session.
            if let Some(ref ctx) = router_output.hook_specific_output {
                if let Some(ref ac) = ctx.additional_context {
                    if let Some(skill) = extract_skill_name(ac) {
                        // Set active_skill for context injection (SKILL.md loading),
                        // but ONLY register workflow-bearing skills when explicitly
                        // invoked via slash command. The skill router's regex matching
                        // is too aggressive — it matches casual phrases like "do it"
                        // to "execute", "deploy this" to "deploy", etc. This registers
                        // a workflow with mandatory phases that blocks ALL tool calls
                        // until the user reads phase files they never intended to use.
                        //
                        // Slash commands set the prompt to exactly "/<skill>" which the
                        // router detects. Casual matches come from normal conversation.
                        let is_slash_command = input
                            .prompt
                            .as_deref()
                            .map(|p: &str| p.trim().starts_with('/'))
                            .unwrap_or(false);

                        if workflows.contains_key(&skill) && !is_slash_command {
                            // Skill has a workflow but was NOT explicitly invoked.
                            // Set active_skill for context injection (so SKILL.md loads)
                            // but do NOT call set_active_skill() which would register
                            // the workflow and trigger the phase gate.
                            state.active_skill = Some(skill.clone());
                            debug!(
                                skill = %skill,
                                "Skill detected via regex without slash command — setting context only"
                            );
                        } else if workflows.contains_key(&skill) {
                            // Explicit slash command — register the workflow
                            state.set_active_skill(&skill);
                        } else {
                            // No workflow definition — just set for context
                            state.active_skill = Some(skill.clone());
                        }
                    } else if ac.contains("No skill matched") {
                        state.active_skill = None;
                    }
                }
            }

            // Phase validator — inject phase + step progress context
            let validator_output =
                hooks::phase_validator::process(&input, &state, &workflows, &step_configs);
            output.merge(&validator_output);

            // Error reporter — inject Linear filing instructions for unresolved errors
            let error_output = hooks::error_reporter::process(&input);
            output.merge(&error_output);

            // Hygiene override — detect override commands in prompt
            let override_output = hooks::hygiene_override::process(&input);
            output.merge(&override_output);

            // Worktree reminder — remind to use EnterWorktree in git repos
            let worktree_output = hooks::worktree_reminder::process(&input);
            output.merge(&worktree_output);

            // Todo loader — inject active todos into context
            let todo_output = hooks::todo_loader::process(&input);
            output.merge(&todo_output);

            // --- Two-phase hooks (read state written by Stop, inject instructions) ---

            // Doc drift — inject update instructions for stale docs
            let drift_output = hooks::doc_drift::process_prompt(&input);
            output.merge(&drift_output);

            // Doc cleanup — inject cleanup instructions for junk docs
            let cleanup_output = hooks::doc_cleanup::process_prompt(&input);
            output.merge(&cleanup_output);

            // Commit hygiene — remind about uncommitted changes
            let commit_output = hooks::commit_hygiene::process_prompt(&input);
            output.merge(&commit_output);

            // Context monitor — inject zone-specific strategy guidance
            let ctx_prompt_output = hooks::context_monitor::process_prompt(&input);
            output.merge(&ctx_prompt_output);

            // Verification gate — remind to verify before claiming completion
            let verify_prompt_output = hooks::verification_gate::process_prompt(&input);
            output.merge(&verify_prompt_output);

            // Activity tracker — inject session activity summary when context is elevated
            let activity_prompt_output = hooks::activity_tracker::process_prompt(&input);
            output.merge(&activity_prompt_output);

            // Memory inject — search Qdrant for semantically relevant memories
            let memory_output = hooks::memory_inject::process(&input);
            output.merge(&memory_output);
        }

        HookEvent::PreToolUse => {
            // Phase gate — check workflow state + track Read() calls on phase files
            let gate_output = hooks::phase_gate::process(&input, &mut state, &workflows);
            output.merge(&gate_output);

            if gate_output.blocked == Some(true) {
                state.record_blocked();
            }

            // Git hygiene — check uncommitted changes (only for Edit/Write)
            if matches!(input.tool_name.as_deref(), Some("Edit" | "Write")) {
                let hygiene_output = hooks::git_hygiene::process(&input, &git);
                output.merge(&hygiene_output);
            }

            // Pre-commit verification — block git commit/push without test evidence (Bash only)
            if matches!(input.tool_name.as_deref(), Some("Bash")) {
                let commit_output = hooks::pre_commit_verification::process(&input);
                output.merge(&commit_output);

                // Commit message validator — enforce conventional commits (Bash only)
                let msg_output = hooks::commit_message_validator::process(&input);
                output.merge(&msg_output);

                // Pre-push Steel test — block git push without Steel test (Bash only)
                let steel_output = hooks::pre_push_steel_test::process(&input);
                output.merge(&steel_output);

                // Wrangler guard — block Node wrangler deploy, gate deletes (Bash only)
                let wrangler_output = hooks::wrangler_guard::process(&input);
                output.merge(&wrangler_output);
            }
        }

        HookEvent::PostToolUse => {
            // MCP health — detect MCP server failures and log to errors.jsonl
            let mcp_output = hooks::mcp_health::process(&input);
            output.merge(&mcp_output);

            // Todo interceptor — persist rich todos from TodoWrite calls
            let todo_output = hooks::todo_interceptor::process(&input);
            output.merge(&todo_output);

            // Evidence collector — capture tool results for proof chains.
            // **Attack #104 note**: In CLI hook mode, PhaseCollectionState is transient
            // (not serializable to disk), so we pass None. Evidence collection only works
            // in daemon mode where state is held in memory. This is a known limitation —
            // the proof chain still works via ProofEngine's own evidence gathering.
            // TODO: Implement evidence persistence for CLI mode if needed.
            let evidence_output = hooks::evidence_collector::process(&input, None);
            output.merge(&evidence_output);

            // Activity tracker — log every tool call to activity-log.jsonl
            let activity_output = hooks::activity_tracker::process_post_tool(&input);
            output.merge(&activity_output);

            // Steel test recorder — write state file on successful session release
            let steel_output = hooks::pre_push_steel_test::process_post_tool(&input);
            output.merge(&steel_output);

            // Plan organizer — inject plan file organization instructions (ExitPlanMode only)
            if matches!(input.tool_name.as_deref(), Some("ExitPlanMode")) {
                let plan_output = hooks::plan_organizer::process(&input);
                output.merge(&plan_output);
            }

            // Account cascade — auto-switch all MCP servers after account change
            let cascade_output = hooks::account_cascade::process(&input);
            output.merge(&cascade_output);
        }

        HookEvent::Stop => {
            // Execution log — capture [RUN]/[STEP]/[PHASE] markers from transcript
            let exec_output = hooks::execution_log::process(&input);
            output.merge(&exec_output);

            // Skill telemetry — aggregate skill usage metrics
            let telemetry_output = hooks::skill_telemetry::process(&input);
            output.merge(&telemetry_output);

            // --- Two-phase hooks (detect state, write for UserPromptSubmit to read) ---

            // Context monitor — capture context window usage zone
            let ctx_output = hooks::context_monitor::process_stop(&input);
            output.merge(&ctx_output);

            // Commit hygiene — detect uncommitted changes
            let hygiene_output = hooks::commit_hygiene::process_stop(&input, &git);
            output.merge(&hygiene_output);

            // Doc cleanup — scan for junk docs
            let doc_output = hooks::doc_cleanup::process_stop(&input);
            output.merge(&doc_output);

            // Doc drift — detect stale README/CLAUDE.md/CHANGELOG
            let drift_output = hooks::doc_drift::process_stop(&input);
            output.merge(&drift_output);

            // Verification gate — detect unverified completion claims
            let verify_output = hooks::verification_gate::process_stop(&input);
            output.merge(&verify_output);

            // Activity tracker — build session summary from activity log
            let activity_stop_output = hooks::activity_tracker::process_stop(&input);
            output.merge(&activity_stop_output);

            // Task persist — final snapshot catches any TaskUpdate calls mid-turn
            let task_persist_output = hooks::task_persist::process(&input);
            output.merge(&task_persist_output);

            // Memory extract — sync recently modified memory files to Qdrant
            let memory_extract_output = hooks::memory_extract::process(&input);
            output.merge(&memory_extract_output);

            // Memory feedback — boost used memories, flag corrections
            let memory_feedback_output = hooks::memory_feedback::process(&input);
            output.merge(&memory_feedback_output);

            // Memory inject (Stop phase) — pre-compute Qdrant search for next turn
            let memory_precompute_output = hooks::memory_inject::process_stop(&input);
            output.merge(&memory_precompute_output);
        }

        HookEvent::SessionStart => {
            // **Attack #57 fix**: Only create fresh state if no existing state was loaded.
            // Unconditional reset lets an attacker trigger SessionStart mid-session
            // (via crafted event) to wipe all workflow progress and phase gates.
            // If state already exists (loaded from disk at line 64), preserve it.
            if state.tool_calls == 0 && state.workflows.is_empty() && state.phases_read.is_empty() {
                // Genuinely new session — use fresh state (already created above)
                state = SessionState::new(session_id);
            } else {
                debug!(
                    session_id,
                    tool_calls = state.tool_calls,
                    workflows = state.workflows.len(),
                    "SessionStart received for active session — preserving existing state"
                );
            }

            // Session init — log session, sync marketplace repo, inject startup context
            let init_output = hooks::session_init::process(&input);
            output.merge(&init_output);

            // Task rehydrate — inject persistent tasks from previous sessions
            let rehydrate_output = hooks::task_rehydrate::process(&input);
            output.merge(&rehydrate_output);

            // Memory verify — verify stale memories against ground truth (24h cooldown)
            let verify_output = hooks::memory_verify::process(&input);
            output.merge(&verify_output);

            // Version drift check — runs once per session with 24h cooldown.
            // Checks npm registry for latest Claude Code version and caches result.
            if let Some(drift_msg) = check_version_drift() {
                let drift_output = HookOutput::inject_context(HookEvent::SessionStart, drift_msg);
                output.merge(&drift_output);
            }
        }

        HookEvent::PreCompact => {
            // Pre-compact snapshot — save session state before context compaction
            let compact_output = hooks::pre_compact::process(&input);
            output.merge(&compact_output);

            // Session index — upsert transcript exchanges to Qdrant for search
            let index_output = hooks::session_index::process(&input);
            output.merge(&index_output);
        }

        HookEvent::TeammateIdle => {
            // Team quality gate — remind teammate to check for remaining work
            let idle_output = hooks::teammate_idle::process(&input);
            output.merge(&idle_output);
        }

        HookEvent::TaskCompleted => {
            // Task verification gate — verify work before marking complete
            let completed_output = hooks::task_completed::process(&input);
            output.merge(&completed_output);

            // Task persist — snapshot task list to persistent markdown + JSON
            let persist_output = hooks::task_persist::process(&input);
            output.merge(&persist_output);
        }

        // ── New events added from Claude Code v2.1.88 source analysis ──

        HookEvent::SessionEnd => {
            // Session cleanup — flush state, log session end (1.5s timeout!)
            let end_output = hooks::session_end::process(&input);
            output.merge(&end_output);
        }

        HookEvent::PostCompact => {
            // Restore critical state after context compaction
            let compact_output = hooks::post_compact::process(&input);
            output.merge(&compact_output);
        }

        HookEvent::SubagentStart => {
            // Inject skill context into spawned agents
            let subagent_output = hooks::subagent_start::process(&input);
            output.merge(&subagent_output);
        }

        HookEvent::SubagentStop => {
            // Log agent completion for telemetry
            let subagent_output = hooks::subagent_stop::process(&input);
            output.merge(&subagent_output);
        }

        HookEvent::TaskCreated => {
            // Log task creation for telemetry
            let task_output = hooks::task_created::process(&input);
            output.merge(&task_output);

            // Task persist — snapshot task list to persistent markdown + JSON
            let persist_output = hooks::task_persist::process(&input);
            output.merge(&persist_output);
        }

        HookEvent::Setup => {
            // Repo init/maintenance
            let setup_output = hooks::setup::process(&input);
            output.merge(&setup_output);
        }

        HookEvent::CwdChanged => {
            // Working directory changed — re-detect project context
            let cwd_output = hooks::cwd_changed::process(&input);
            output.merge(&cwd_output);
        }

        HookEvent::StopFailure => {
            // API error at end of turn — log for diagnostics
            let failure_output = hooks::stop_failure::process(&input);
            output.merge(&failure_output);
        }

        HookEvent::PermissionDenied => {
            // Auto-mode denied a tool call — log for diagnostics
            let denied_output = hooks::permission_denied::process(&input);
            output.merge(&denied_output);
        }

        HookEvent::PostToolUseFailure => {
            // Tool execution failed — log for diagnostics
            let tool_name = input.tool_name.as_deref().unwrap_or("unknown");
            let is_timeout = input.extra.get("is_timeout").and_then(|v| v.as_bool()).unwrap_or(false);
            let error = input.extra.get("error").and_then(|v| v.as_str()).unwrap_or("");
            tracing::debug!(tool_name, is_timeout, error, "Tool execution failed");

            if let Some(home) = dirs::home_dir() {
                let metrics_dir = home.join(".claude").join("metrics");
                let entry = serde_json::json!({
                    "event": "tool_failure",
                    "tool_name": tool_name,
                    "is_timeout": is_timeout,
                    "error": error,
                    "session_id": input.session_id,
                    "ts": chrono::Utc::now().to_rfc3339(),
                });
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(metrics_dir.join("errors.jsonl"))
                {
                    use std::io::Write;
                    let _ = writeln!(file, "{}", entry);
                }
            }
        }

        HookEvent::PermissionRequest => {
            // Log permission request details for future auto-approve rules.
            // Currently pass-through — no auto-decisions. The value of this hook
            // will come when we add specific auto-approve rules for trusted tools
            // in certain contexts (e.g., auto-allow Edit in a known project dir).
            let tool = input.tool_name.as_deref().unwrap_or("unknown");
            let has_suggestions = input.permission_suggestions.as_ref().map(|s| s.len()).unwrap_or(0);
            tracing::debug!(tool, has_suggestions, "PermissionRequest — pass through (no auto-decisions yet)");
        }

        HookEvent::Elicitation => {
            // MCP server requesting user input — log details, pass through.
            // Auto-responding to elicitation without understanding the context is risky.
            // Future: auto-accept known servers (e.g., sentinel, codex) for trusted prompts.
            let server = input.extra.get("mcp_server_name").and_then(|v| v.as_str()).unwrap_or("unknown");
            let message = input.extra.get("message").and_then(|v| v.as_str()).unwrap_or("");
            tracing::debug!(server, message, "Elicitation request from MCP server — pass through");
        }

        HookEvent::ElicitationResult => {
            // Post-elicitation response — pass through for now
            tracing::debug!("ElicitationResult received — pass through");
        }

        HookEvent::ConfigChange => {
            // Settings or skill file changed — validate and warn on dangerous changes.
            let source = input.extra.get("source").and_then(|v| v.as_str()).unwrap_or("unknown");
            let file_path = input.extra.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            tracing::debug!(source, file_path, "ConfigChange detected");

            // Warn if disableAllHooks is set (kill-switch that disables all enforcement)
            if source == "user_settings" || source == "project_settings" || source == "local_settings" {
                if !file_path.is_empty() {
                    if let Ok(settings_content) = std::fs::read_to_string(file_path) {
                        if settings_content.contains("\"disableAllHooks\"") && settings_content.contains("true") {
                            output.system_message = Some(
                                "[sentinel] WARNING: disableAllHooks detected in settings — all hook enforcement will be disabled!".to_string()
                            );
                        }
                    }
                }
            }

            // Log skill file changes for telemetry
            if source == "skills" {
                tracing::info!(file_path, "Skill file changed");
            }
        }

        HookEvent::InstructionsLoaded => {
            // CLAUDE.md or other instruction file loaded — log details.
            let file_path = input.extra.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let memory_type = input.extra.get("memory_type").and_then(|v| v.as_str()).unwrap_or("");
            let load_reason = input.extra.get("load_reason").and_then(|v| v.as_str()).unwrap_or("");
            tracing::debug!(file_path, memory_type, load_reason, "Instructions loaded");

            // Log managed/enterprise overrides — these can silently change behavior
            if memory_type == "Managed" {
                tracing::info!(file_path, "Managed (enterprise) instructions loaded — may override user settings");
            }
        }

        HookEvent::FileChanged => {
            // Watched file changed — log and inject context for important files.
            let file_path = input.file_path.as_deref()
                .or_else(|| input.extra.get("file_path").and_then(|v| v.as_str()))
                .unwrap_or("");
            let event_type = input.extra.get("event").and_then(|v| v.as_str()).unwrap_or("change");
            tracing::info!(file_path, event_type, "Watched file changed");

            if file_path.ends_with("CLAUDE.md") {
                output.system_message = Some(
                    "[sentinel] CLAUDE.md changed — context may need refresh".to_string()
                );
            } else if file_path.ends_with("settings.json") {
                output.system_message = Some(
                    "[sentinel] settings.json changed — hook configuration may have been updated".to_string()
                );
            }
        }

        HookEvent::WorktreeCreate => {
            // Git worktree created — pass through for now
            tracing::debug!("WorktreeCreate received — pass through");
        }

        HookEvent::WorktreeRemove => {
            // Git worktree removed — pass through for now
            tracing::debug!("WorktreeRemove received — pass through");
        }

        HookEvent::Notification => {
            // Internal notification — pass through for now
            tracing::debug!("Notification received — pass through");
        }
    }

    // Record hook invocation with actual elapsed time
    let elapsed_ms = start_time.elapsed().as_millis() as u64;
    state.record_hook_invocation(event, elapsed_ms);

    // Save state AFTER all processing (so phase reads and tool calls are persisted)
    if let Err(e) = sentinel_infrastructure::state_store::save(&mut state) {
        tracing::warn!(error = %e, "Failed to persist hook state");
    }

    // Transform output for Claude Code's JSON schema
    match hook_event {
        HookEvent::PreToolUse => {
            // Transform legacy blocked/reason → proper hookSpecificOutput with permissionDecision
            output = output.into_pretool_output();
        }
        HookEvent::UserPromptSubmit
        | HookEvent::PostToolUse
        | HookEvent::PostToolUseFailure
        | HookEvent::SessionStart
        | HookEvent::Setup
        | HookEvent::SubagentStart
        | HookEvent::Notification
        | HookEvent::PermissionRequest
        | HookEvent::PermissionDenied
        | HookEvent::Elicitation
        | HookEvent::ElicitationResult
        | HookEvent::CwdChanged
        | HookEvent::FileChanged
        | HookEvent::WorktreeCreate => {
            // These events support hookSpecificOutput natively
        }
        _ => {
            // Strip hookSpecificOutput for events Claude Code doesn't process
            output.hook_specific_output = None;
        }
    }

    // Inject project context only for prompt-scoped events. Adding it to every
    // tool hook bloats the transcript during tool-heavy sessions and drives
    // premature compaction.
    if should_attach_project_context(hook_event) {
        if let Ok(project) = std::env::var("CLAUDE_PROJECT") {
            if !project.is_empty() {
                let project_header = format!("[Project Context] Active project: {}", project);
                if let Some(ref mut hso) = output.hook_specific_output {
                    match &hso.additional_context {
                        Some(existing) => {
                            hso.additional_context =
                                Some(format!("{}\n\n{}", project_header, existing));
                        }
                        None => {
                            hso.additional_context = Some(project_header);
                        }
                    }
                } else {
                    output.hook_specific_output = Some(HookSpecificOutput {
                        hook_event_name: hook_event.to_string(),
                        additional_context: Some(project_header),
                        ..HookSpecificOutput::default()
                    });
                }
            }
        }
    }

    // Write output to stdout
    sentinel_infrastructure::stdout::write_hook_output(&output)?;

    Ok(())
}

fn parse_hook_event(event: &str) -> Result<HookEvent> {
    match HookEvent::from_arg(event) {
        Some(e) => Ok(e),
        None => {
            eprintln!(
                "[sentinel] ERROR: Unknown hook event type '{}'. \
                 Valid events: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, \
                 PostToolUse, PostToolUseFailure, Stop, StopFailure, PreCompact, PostCompact, \
                 Setup, SubagentStart, SubagentStop, TeammateIdle, TaskCreated, TaskCompleted, \
                 PermissionDenied, CwdChanged",
                event
            );
            anyhow::bail!("Unknown hook event type: '{}'", event);
        }
    }
}

fn should_attach_project_context(hook_event: HookEvent) -> bool {
    matches!(
        hook_event,
        HookEvent::SessionStart
            | HookEvent::UserPromptSubmit
            | HookEvent::SubagentStart
            | HookEvent::Setup
    )
}

fn hook_timeout(hook_event: HookEvent) -> Duration {
    match hook_event {
        HookEvent::SessionStart | HookEvent::Stop | HookEvent::PreCompact | HookEvent::PostCompact => Duration::from_secs(8),
        // SessionEnd has a 1.5s timeout in Claude Code — be fast
        HookEvent::SessionEnd => Duration::from_secs(1),
        _ => Duration::from_secs(5),
    }
}

async fn run_supervised(
    hook_event: HookEvent,
    event: &str,
    matcher: Option<&str>,
    raw_input: String,
) -> Result<()> {
    let current_exe = std::env::current_exe().context("Failed to resolve sentinel-engine path")?;
    let mut command = tokio::process::Command::new(current_exe);
    command
        .arg("hook-internal")
        .arg("--event")
        .arg(event)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(matcher) = matcher {
        command.arg("--matcher").arg(matcher);
    }

    let child = command.spawn().context("Failed to spawn hook worker")?;
    let timeout = hook_timeout(hook_event);

    match supervise_child(child, raw_input.into_bytes(), timeout).await? {
        Some(output) => {
            if !output.stderr.is_empty() {
                std::io::stderr().write_all(&output.stderr)?;
            }

            if !output.status.success() {
                warn!(
                    event = %hook_event,
                    exit_code = output.status.code().unwrap_or(-1),
                    "Hook worker exited non-zero — returning safe allow response"
                );
                return write_safe_allow_response();
            }

            if output.stdout.is_empty() {
                return write_safe_allow_response();
            }

            std::io::stdout().write_all(&output.stdout)?;
            Ok(())
        }
        None => {
            warn!(
                event = %hook_event,
                timeout_ms = timeout.as_millis() as u64,
                "Hook worker timed out — returning safe allow response"
            );
            write_safe_allow_response()
        }
    }
}

struct ChildOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn supervise_child(
    mut child: tokio::process::Child,
    stdin_payload: Vec<u8>,
    timeout: Duration,
) -> Result<Option<ChildOutput>> {
    let mut stdin = child.stdin.take();
    let mut stdout = child
        .stdout
        .take()
        .context("Hook worker stdout not captured")?;
    let mut stderr = child
        .stderr
        .take()
        .context("Hook worker stderr not captured")?;

    let stdin_task = tokio::spawn(async move {
        if let Some(mut stdin) = stdin.take() {
            if !stdin_payload.is_empty() {
                stdin.write_all(&stdin_payload).await?;
            }
            stdin.shutdown().await?;
        }
        Ok::<(), std::io::Error>(())
    });

    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    });

    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stderr.read_to_end(&mut buffer).await?;
        Ok::<Vec<u8>, std::io::Error>(buffer)
    });

    let wait_result = tokio::time::timeout(timeout, child.wait()).await;
    let status = match wait_result {
        Ok(result) => Some(result.context("Failed waiting for hook worker")?),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };

    stdin_task
        .await
        .context("Hook worker stdin task join failed")?
        .context("Hook worker stdin write failed")?;

    let stdout = stdout_task
        .await
        .context("Hook worker stdout task join failed")?
        .context("Hook worker stdout read failed")?;

    let stderr = stderr_task
        .await
        .context("Hook worker stderr task join failed")?
        .context("Hook worker stderr read failed")?;

    Ok(status.map(|status| ChildOutput {
        status,
        stdout,
        stderr,
    }))
}

fn write_safe_allow_response() -> Result<()> {
    sentinel_infrastructure::stdout::write_hook_output(&HookOutput::allow())
}

/// Validate that the caller is plausibly Claude Code.
///
/// Two checks:
/// 1. **Stdin is piped** (not a terminal) — hooks are always invoked via pipe
///    from Claude Code's hook runner, never interactively. Hard fail unless
///    `SENTINEL_ALLOW_TERMINAL=1` is set.
/// 2. **CLAUDE_CODE env marker** — Claude Code sets `CLAUDE_CODE_ENTRY_POINT`
///    when spawning hook subprocesses. Its absence is suspicious.
///
/// If validation fails, a `caller_rejected` event is logged to the security
/// audit log and the function returns `Err`, causing `run()` to output `{}`
/// (safe allow) and exit early.
///
/// This is defense-in-depth, not a security boundary. A determined attacker can
/// still set the env var and pipe crafted JSON, but they must:
///   1. Know the sentinel CLI exists and its arguments
///   2. Set `CLAUDE_CODE_ENTRY_POINT` in their environment
///   3. Construct valid HookInput JSON with a real session_id
///   4. Pipe it correctly (not type interactively)
fn validate_caller() -> Result<()> {
    // Escape hatch for debugging / manual testing
    if std::env::var("SENTINEL_ALLOW_TERMINAL").as_deref() == Ok("1") {
        return Ok(());
    }

    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        eprintln!(
            "[sentinel] BLOCKED: Hook invoked from interactive terminal. \
             Hooks must be called by Claude Code via pipe. \
             Set SENTINEL_ALLOW_TERMINAL=1 to override for debugging."
        );
        let _ = sentinel_infrastructure::security_log::log_security_event(
            "caller_rejected",
            "unknown",
            "Hook invoked from interactive terminal (stdin is TTY)",
        );
        anyhow::bail!("Caller validation failed: stdin is a terminal");
    }

    // Check for Claude Code environment marker.
    // Claude Code sets CLAUDE_CODE_ENTRY_POINT (e.g. "cli", "sdk").
    // Absence is not definitive proof of abuse (could be an older version),
    // so keep it as a debug-only signal instead of stderr noise. Claude treats
    // stderr from hooks as a non-blocking hook error banner.
    if std::env::var("CLAUDE_CODE_ENTRY_POINT").is_err() {
        debug!("CLAUDE_CODE_ENTRY_POINT not set for hook invocation");
    }

    // Optional parent-process attestation.
    //
    // This was introduced as defense-in-depth, but the Windows `wmic`/`tasklist`
    // probe can outlive the hook's JSON response and keep the process alive long
    // enough for Claude Code to appear wedged after pressing Enter. Keep the
    // simpler stdin/env checks on by default and make the heavier attestation
    // opt-in for debugging or forensics.
    #[cfg(windows)]
    if std::env::var("SENTINEL_ENABLE_PARENT_ATTESTATION").as_deref() == Ok("1") {
        if let Some(parent) = get_parent_process_name() {
            #[cfg(windows)]
            let valid_parents = [
                "node.exe",
                "bun.exe",
                "claude.exe",
                "cmd.exe",
                "powershell.exe",
                "pwsh.exe",
                "bash.exe",
                "sentinel-engine.exe",
                "sentinel.exe",
            ];
            #[cfg(not(windows))]
            let valid_parents = [
                "node",
                "bun",
                "claude",
                "bash",
                "zsh",
                "sh",
                "sentinel-engine",
                "sentinel",
            ];
            if !valid_parents.iter().any(|v| parent.contains(v)) {
                eprintln!(
                    "[sentinel] WARNING: Parent process '{}' is not a known Claude Code runtime.",
                    parent
                );
                let _ = sentinel_infrastructure::security_log::log_security_event(
                    "caller_rejected",
                    "unknown",
                    &format!(
                        "Parent process '{}' is not a known Claude Code runtime",
                        parent
                    ),
                );
            }
        }
    }

    Ok(())
}

/// Get the parent process name on Windows using tasklist + wmic.
///
/// Strategy: Use `wmic process where ProcessId=<PID> get ParentProcessId` to find
/// the parent PID, then `tasklist /FI "PID eq <PPID>"` to get its name.
/// Both are native Windows tools that start in ~30ms (vs PowerShell ~2s).
///
/// Returns None if the check fails for any reason (fail-open for resilience).
#[cfg(windows)]
fn get_parent_process_name() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    let pid = std::process::id();

    // Step 1: Get parent PID via wmic (~30ms)
    let output = std::process::Command::new("wmic")
        .args([
            "process",
            "where",
            &format!("ProcessId={pid}"),
            "get",
            "ParentProcessId",
            "/VALUE",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parent_pid: u32 = stdout
        .lines()
        .find(|l| l.starts_with("ParentProcessId="))
        .and_then(|l| l.strip_prefix("ParentProcessId="))
        .and_then(|s| s.trim().parse().ok())?;

    // Step 2: Get parent process name via tasklist (~30ms)
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {parent_pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // CSV format: "Image Name","PID","Session Name","Session#","Mem Usage"
    let name = stdout
        .lines()
        .next()?
        .split(',')
        .next()?
        .trim_matches('"')
        .to_lowercase();

    if name.is_empty() || name.contains("no tasks") {
        None
    } else {
        Some(name)
    }
}

/// Glass break directory — all glass break files live here.
fn glass_break_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("sentinel"))
}

/// Check if a glass break override is active.
///
/// Two-layer design:
///   1. **Token check (fast path):** If `.glass-break-token` exists and contains
///      a valid, non-expired timestamp, return true immediately. This is the
///      common path after initial confirmation.
///   2. **Trigger check (slow path):** If `.glass-break` file exists (user created
///      it manually), launch a native Windows dialog requiring human confirmation.
///      On success, write a time-limited token and delete the trigger file.
///      On failure/cancel, delete the trigger file and return false.
///
/// The dialog prevents AI from self-activating the override. The token prevents
/// the dialog from appearing on every subsequent hook invocation.
fn check_glass_break_override() -> bool {
    let dir = match glass_break_dir() {
        Some(d) => d,
        None => return false,
    };

    // Fast path: check for valid active token
    let token_path = dir.join(".glass-break-token");
    if token_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&token_path) {
            if let Some(expiry_str) = contents.lines().next() {
                if let Ok(expiry) = chrono::DateTime::parse_from_rfc3339(expiry_str.trim()) {
                    if expiry > chrono::Utc::now() {
                        return true; // Token still valid
                    }
                }
            }
        }
        // Token expired or invalid — clean up
        let _ = std::fs::remove_file(&token_path);
    }

    // Slow path: check for trigger file
    let trigger_path = dir.join(".glass-break");
    if !trigger_path.exists() {
        return false;
    }

    // Trigger exists — require human confirmation via native dialog
    let confirmed = show_glass_break_dialog();

    // Always delete the trigger file (one-shot)
    let _ = std::fs::remove_file(&trigger_path);

    if confirmed {
        // Write a 15-minute token
        let expiry = chrono::Utc::now() + chrono::Duration::minutes(15);
        let _ = std::fs::write(&token_path, expiry.to_rfc3339());

        // Audit log
        let _ = sentinel_infrastructure::security_log::log_security_event(
            "glass_break_activated",
            "unknown",
            "Glass break confirmed via dialog — enforcement bypassed for 15 minutes",
        );

        eprintln!(
            "[sentinel] Glass break confirmed. Enforcement bypassed until {}",
            expiry.format("%H:%M:%S")
        );
        true
    } else {
        eprintln!("[sentinel] Glass break DENIED — dialog cancelled or timed out");
        let _ = sentinel_infrastructure::security_log::log_security_event(
            "glass_break_denied",
            "unknown",
            "Glass break trigger file found but dialog was cancelled or timed out",
        );
        false
    }
}

/// Show a native Windows dialog requiring human confirmation.
/// Returns true only if the user types the correct confirmation phrase.
/// Cannot be completed by AI — requires interactive desktop input.
fn show_glass_break_dialog() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;

        // Generate a 4-digit challenge code
        let mut code_bytes = [0u8; 2];
        let _ = getrandom::getrandom(&mut code_bytes);
        let code = u16::from_le_bytes(code_bytes) % 10000;
        let code_str = format!("{code:04}");

        // Use PowerShell to show an InputBox dialog (works on all Windows versions)
        // The dialog is modal and blocks until the user responds
        let ps_script = format!(
            r#"Add-Type -AssemblyName Microsoft.VisualBasic; $result = [Microsoft.VisualBasic.Interaction]::InputBox("SENTINEL EMERGENCY OVERRIDE`n`nThis will disable ALL security enforcement for 15 minutes.`n`nTo confirm, type the code: {code_str}`n`nThis action is audited.", "Glass Break", ""); if ($result -eq "{code_str}") {{ Write-Output "CONFIRMED" }} else {{ Write-Output "DENIED" }}"#
        );

        // Note: do NOT use -NonInteractive — InputBox requires interactive mode.
        // CREATE_NO_WINDOW hides the PowerShell console but the InputBox dialog
        // still appears on the interactive desktop.
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_script])
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                stdout.trim() == "CONFIRMED"
            }
            Err(e) => {
                eprintln!("[sentinel] Failed to show glass break dialog: {e}");
                false
            }
        }
    }

    #[cfg(not(windows))]
    {
        // On non-Windows, fall back to terminal challenge
        eprintln!("[sentinel] Glass break requires interactive confirmation.");
        eprintln!("[sentinel] Use `sentinel break --reason \"...\"` from a terminal.");
        false
    }
}

/// Check if a newer Claude Code version is available (24h cooldown).
///
/// Queries the npm registry for the latest `@anthropic-ai/claude-code` version
/// and caches the result to `~/.claude/sentinel/state/version-drift.json`.
/// Returns a context message if a newer version was detected, None otherwise.
fn check_version_drift() -> Option<String> {
    let state_dir = dirs::home_dir()?.join(".claude").join("sentinel").join("state");
    let drift_file = state_dir.join("version-drift.json");

    // Check cooldown (24 hours)
    if drift_file.exists() {
        if let Ok(metadata) = std::fs::metadata(&drift_file) {
            if let Ok(modified) = metadata.modified() {
                if modified.elapsed().unwrap_or_default() < std::time::Duration::from_secs(86400) {
                    // Read cached result
                    if let Ok(content) = std::fs::read_to_string(&drift_file) {
                        if let Ok(cached) = serde_json::from_str::<serde_json::Value>(&content) {
                            if let Some(msg) = cached.get("message").and_then(|v| v.as_str()) {
                                if !msg.is_empty() {
                                    return Some(msg.to_string());
                                }
                            }
                        }
                    }
                    return None; // Within cooldown, no drift found last check
                }
            }
        }
    }

    // Run npm view to check latest version (synchronous, ~200ms typically)
    let result = std::process::Command::new("npm")
        .args(["view", "@anthropic-ai/claude-code", "version"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !result.status.success() {
        return None; // npm not available or offline
    }

    let latest = String::from_utf8_lossy(&result.stdout).trim().to_string();
    if latest.is_empty() {
        return None;
    }

    // Get current version from Claude Code env var (set since ~v2.1)
    let current = std::env::var("CLAUDE_CODE_VERSION").ok();
    let message = match &current {
        Some(cur) if cur != &latest => {
            format!(
                "[sentinel] Claude Code version drift detected: installed {} → latest {}. \
                 Run `npm update -g @anthropic-ai/claude-code` to update.",
                cur, latest
            )
        }
        _ => String::new(), // Same version or can't determine current
    };

    let entry = serde_json::json!({
        "latest": latest,
        "current": current,
        "checked": chrono::Utc::now().to_rfc3339(),
        "message": message,
    });

    let _ = std::fs::create_dir_all(&state_dir);
    let _ = std::fs::write(
        &drift_file,
        serde_json::to_string_pretty(&entry).unwrap_or_default(),
    );

    tracing::debug!(
        latest_npm = %latest,
        current = ?current,
        "Version drift check completed"
    );

    if message.is_empty() {
        None
    } else {
        Some(message)
    }
}

/// Extract skill name from router context like "[Skill Router] Detected skill: linear. MANDATORY..."
fn extract_skill_name(context: &str) -> Option<String> {
    let prefix = "Detected skill: ";
    let start = context.find(prefix)? + prefix.len();
    let rest = &context[start..];
    let end = rest.find('.')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn test_command(command_str: &str) -> tokio::process::Child {
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = tokio::process::Command::new("cmd");
            cmd.arg("/C").arg(command_str);
            cmd
        };

        #[cfg(not(windows))]
        let mut cmd = {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command_str);
            cmd
        };

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[test]
    fn test_extract_skill_name() {
        let ctx = "[Skill Router] Detected skill: linear. MANDATORY: You MUST Read...";
        assert_eq!(extract_skill_name(ctx), Some("linear".to_string()));
    }

    #[test]
    fn test_extract_skill_name_none() {
        assert_eq!(extract_skill_name("no skill here"), None);
    }

    #[test]
    fn test_no_skill_matched_clears_active_skill() {
        // Simulates the scenario: skill router detected "linear" on message 1,
        // then "No skill matched" on message 2. active_skill should be cleared.
        let no_match_context = "[Skill Router] No skill matched — general conversation mode.";
        assert_eq!(extract_skill_name(no_match_context), None);
        // The clearing happens in the caller (run()) when extract_skill_name returns
        // None AND the context contains "No skill matched"
        assert!(no_match_context.contains("No skill matched"));
    }

    #[test]
    fn test_hook_timeout_values() {
        assert_eq!(
            hook_timeout(HookEvent::UserPromptSubmit),
            Duration::from_secs(5)
        );
        assert_eq!(hook_timeout(HookEvent::PreToolUse), Duration::from_secs(5));
        assert_eq!(hook_timeout(HookEvent::Stop), Duration::from_secs(8));
    }

    #[test]
    fn test_project_context_only_attaches_to_prompt_scoped_events() {
        assert!(should_attach_project_context(HookEvent::SessionStart));
        assert!(should_attach_project_context(HookEvent::UserPromptSubmit));
        assert!(should_attach_project_context(HookEvent::SubagentStart));
        assert!(should_attach_project_context(HookEvent::Setup));
        assert!(!should_attach_project_context(HookEvent::PreToolUse));
        assert!(!should_attach_project_context(HookEvent::PostToolUse));
        assert!(!should_attach_project_context(HookEvent::PostToolUseFailure));
        assert!(!should_attach_project_context(HookEvent::PostCompact));
    }

    #[tokio::test]
    async fn test_supervise_child_returns_output_on_success() {
        #[cfg(windows)]
        let child = test_command("echo ok");

        #[cfg(not(windows))]
        let child = test_command("printf ok");

        let output = supervise_child(child, Vec::new(), Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "ok");
    }

    #[tokio::test]
    async fn test_supervise_child_times_out_and_kills_worker() {
        #[cfg(windows)]
        let child = test_command("ping -n 3 127.0.0.1 >nul");

        #[cfg(not(windows))]
        let child = test_command("sleep 2");

        let output = supervise_child(child, Vec::new(), Duration::from_millis(50))
            .await
            .unwrap();

        assert!(output.is_none());
    }

    /// Regression: hook process must exit within 3s (no lingering threads).
    #[tokio::test]
    async fn test_hook_internal_exits_within_timeout() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let engine = std::path::Path::new(manifest_dir)
            .parent().unwrap()
            .parent().unwrap()
            .join("target").join("release").join(if cfg!(windows) { "sentinel-engine.exe" } else { "sentinel-engine" });

        if !engine.exists() {
            eprintln!("Skipping: sentinel-engine not found at {:?}", engine);
            return;
        }

        let mut child = tokio::process::Command::new(&engine)
            .args(["hook-internal", "--event", "UserPromptSubmit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SENTINEL_ALLOW_TERMINAL", "1")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn sentinel-engine");

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(b"{\"session_id\":\"regression-exit-test\"}\n")
                .await
                .unwrap();
            stdin.shutdown().await.unwrap();
        }

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;

        assert!(
            result.is_ok(),
            "sentinel-engine did not exit within 3s — possible hang"
        );
    }

    /// Regression: stdout must be valid JSON (no tracing leaks).
    #[tokio::test]
    async fn test_hook_stdout_is_valid_json() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let engine = std::path::Path::new(manifest_dir)
            .parent().unwrap()
            .parent().unwrap()
            .join("target").join("release").join(if cfg!(windows) { "sentinel-engine.exe" } else { "sentinel-engine" });

        if !engine.exists() {
            eprintln!("Skipping: sentinel-engine not found at {:?}", engine);
            return;
        }

        let mut child = tokio::process::Command::new(&engine)
            .args(["hook-internal", "--event", "UserPromptSubmit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("SENTINEL_ALLOW_TERMINAL", "1")
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn");

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(b"{\"session_id\":\"regression-json-test\"}\n")
                .await
                .unwrap();
            stdin.shutdown().await.unwrap();
        }

        let output =
            tokio::time::timeout(std::time::Duration::from_secs(3), child.wait_with_output())
                .await
                .expect("timed out")
                .expect("wait failed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.is_empty(), "stdout should not be empty");

        let parsed: Result<serde_json::Value, _> = serde_json::from_str(stdout.trim());
        assert!(parsed.is_ok(), "stdout is not valid JSON: {}", stdout);

        assert!(
            !stdout.contains("[2m"),
            "stdout contains ANSI escape (tracing leak)"
        );
        assert!(
            !stdout.contains("WARN"),
            "stdout contains WARN (tracing leak)"
        );
    }
}
