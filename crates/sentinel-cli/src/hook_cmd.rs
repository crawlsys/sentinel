//! `sentinel hook` — Process hook events (thin client or standalone)

use std::collections::HashMap;
use std::io::Write;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use sentinel_application::hooks;
use sentinel_domain::events::{HookEvent, HookOutput};
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
}

pub async fn run(event: &str, matcher: Option<&str>, standalone: bool) -> Result<()> {
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
                    tracing::warn!("Skill router timed out (5s) — falling back to regex-only");
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
                        // **Attack #58 fix**: Only accept skill names that exist in the
                        // loaded workflows map. Accepting arbitrary names lets an attacker
                        // inject a fake skill with no phases, creating a workflow entry
                        // that has zero enforcement gates.
                        if workflows.contains_key(&skill) {
                            state.set_active_skill(&skill);
                        } else {
                            // **Attack #80 fix**: Do NOT set active_skill for non-workflow skills.
                            // Setting active_skill to a skill with no workflow definition creates
                            // a state where gate.rs sees an active skill, checks for its workflow
                            // (finds None), and falls through to Allow — bypassing any incomplete
                            // workflow that should still be enforced. Instead, leave active_skill
                            // unchanged so find_incomplete_workflow() continues enforcing.
                            eprintln!(
                                "[sentinel] Skill '{}' has no workflow definition — not setting as active_skill \
                                 to preserve existing gate enforcement.",
                                skill
                            );
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
                eprintln!(
                    "[sentinel] SessionStart received for active session '{}' — preserving existing state \
                     ({} tool calls, {} workflows). This may indicate a mid-session reset attempt.",
                    session_id, state.tool_calls, state.workflows.len()
                );
            }

            // Session init — log session, sync marketplace repo, inject startup context
            let init_output = hooks::session_init::process(&input);
            output.merge(&init_output);
        }

        HookEvent::PreCompact => {
            // Pre-compact snapshot — save session state before context compaction
            let compact_output = hooks::pre_compact::process(&input);
            output.merge(&compact_output);
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
        HookEvent::UserPromptSubmit | HookEvent::PostToolUse => {
            // These events support hookSpecificOutput natively — no transform needed
        }
        _ => {
            // Strip hookSpecificOutput for events Claude Code doesn't support
            output.hook_specific_output = None;
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
                 Valid events: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, \
                 Stop, PreCompact, TeammateIdle, TaskCompleted",
                event
            );
            anyhow::bail!("Unknown hook event type: '{}'", event);
        }
    }
}

fn hook_timeout(hook_event: HookEvent) -> Duration {
    match hook_event {
        HookEvent::SessionStart | HookEvent::Stop | HookEvent::PreCompact => Duration::from_secs(8),
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
    // so we only warn — the stdin pipe check above is the hard gate.
    if std::env::var("CLAUDE_CODE_ENTRY_POINT").is_err() {
        eprintln!(
            "[sentinel] WARNING: CLAUDE_CODE_ENTRY_POINT not set. \
             This hook may not have been invoked by Claude Code."
        );
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
            if !valid_parents.iter().any(|v| parent.contains(v)) {
                eprintln!(
                    "[sentinel] WARNING: Parent process '{}' is not a known Claude Code runtime.",
                    parent
                );
                let _ = sentinel_infrastructure::security_log::log_security_event(
                    "caller_rejected",
                    "unknown",
                    &format!("Parent process '{}' is not a known Claude Code runtime", parent),
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
        .args(["process", "where", &format!("ProcessId={pid}"), "get", "ParentProcessId", "/VALUE"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parent_pid: u32 = stdout.lines()
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
    let name = stdout.lines()
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
            .join("target").join("release").join("sentinel-engine.exe");

        if !engine.exists() {
            eprintln!("Skipping: sentinel-engine.exe not found at {:?}", engine);
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
            stdin.write_all(b"{\"session_id\":\"regression-exit-test\"}\n").await.unwrap();
            stdin.shutdown().await.unwrap();
        }

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            child.wait(),
        ).await;

        assert!(result.is_ok(), "sentinel-engine did not exit within 3s — possible hang");
    }

    /// Regression: stdout must be valid JSON (no tracing leaks).
    #[tokio::test]
    async fn test_hook_stdout_is_valid_json() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let engine = std::path::Path::new(manifest_dir)
            .parent().unwrap()
            .parent().unwrap()
            .join("target").join("release").join("sentinel-engine.exe");

        if !engine.exists() {
            eprintln!("Skipping: sentinel-engine.exe not found at {:?}", engine);
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
            stdin.write_all(b"{\"session_id\":\"regression-json-test\"}\n").await.unwrap();
            stdin.shutdown().await.unwrap();
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            child.wait_with_output(),
        ).await.expect("timed out").expect("wait failed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.is_empty(), "stdout should not be empty");

        let parsed: Result<serde_json::Value, _> = serde_json::from_str(stdout.trim());
        assert!(parsed.is_ok(), "stdout is not valid JSON: {}", stdout);

        assert!(!stdout.contains("[2m"), "stdout contains ANSI escape (tracing leak)");
        assert!(!stdout.contains("WARN"), "stdout contains WARN (tracing leak)");
    }
}
