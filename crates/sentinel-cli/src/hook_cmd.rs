//! `sentinel hook` — Process hook events (thin client or standalone)

use std::collections::HashMap;

use anyhow::Result;
use tracing::debug;

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
    let start_time = std::time::Instant::now();
    debug!(event, ?matcher, standalone, "Processing hook event");

    // Parse event type
    let hook_event = HookEvent::from_arg(event)
        .unwrap_or_else(|| {
            debug!("Unknown event type '{}', defaulting to Stop", event);
            HookEvent::Stop
        });

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

    // Load or create session state
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let mut state = sentinel_infrastructure::state_store::load(session_id)?
        .unwrap_or_else(|| SessionState::new(session_id));

    let git = RealGit;

    // Process through matching hooks based on event type
    let mut output = HookOutput::allow();

    match hook_event {
        HookEvent::UserPromptSubmit => {
            // Skill router — classify and route to matching skill
            let router = hooks::skill_router::default_router();
            let router_output = hooks::skill_router::process(&input, &router);
            output.merge(&router_output);

            // Extract detected skill from router output and update state
            if let Some(ref ctx) = router_output.hook_specific_output {
                if let Some(ref ac) = ctx.additional_context {
                    if let Some(skill) = extract_skill_name(ac) {
                        state.set_active_skill(&skill);
                    }
                }
            }

            // Phase validator — inject phase + step progress context
            let validator_output = hooks::phase_validator::process(&input, &state, &workflows, &step_configs);
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

            // Evidence collector — capture tool results for proof chains
            // Passes None when no active phase collection (gracefully skips)
            let evidence_output = hooks::evidence_collector::process(&input, None);
            output.merge(&evidence_output);

            // Activity tracker — log every tool call to activity-log.jsonl
            let activity_output = hooks::activity_tracker::process_post_tool(&input);
            output.merge(&activity_output);

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
            // Initialize fresh state
            state = SessionState::new(session_id);

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
    if let Err(e) = sentinel_infrastructure::state_store::save(&state) {
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

    #[test]
    fn test_extract_skill_name() {
        let ctx = "[Skill Router] Detected skill: linear. MANDATORY: You MUST Read...";
        assert_eq!(extract_skill_name(ctx), Some("linear".to_string()));
    }

    #[test]
    fn test_extract_skill_name_none() {
        assert_eq!(extract_skill_name("no skill here"), None);
    }
}
