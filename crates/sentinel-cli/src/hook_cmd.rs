//! `sentinel hook` — Process hook events (thin client or standalone)

use std::collections::HashMap;

use anyhow::Result;
use tracing::debug;

use sentinel_application::hooks;
use sentinel_domain::events::{HookEvent, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;

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

    // Load or create session state
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let mut state = sentinel_infrastructure::state_store::load(session_id)?
        .unwrap_or_else(|| SessionState::new(session_id));

    let git = RealGit;

    // Process through matching hooks based on event type
    let mut output = HookOutput::allow();

    match hook_event {
        HookEvent::UserPromptSubmit => {
            // Skill router
            let router = hooks::skill_router::default_router();
            let router_output = hooks::skill_router::process(&input, &router);
            output.merge(&router_output);

            // Extract detected skill from router output and update state
            if let Some(ref ctx) = router_output.hook_specific_output {
                if let Some(skill) = extract_skill_name(&ctx.additional_context) {
                    state.set_active_skill(&skill);
                }
            }

            // Phase validator — inject phase progress context
            let validator_output = hooks::phase_validator::process(&input, &state, &workflows);
            output.merge(&validator_output);
        }

        HookEvent::PreToolUse => {
            // Phase gate — check workflow state + track Read() calls on phase files
            // NOTE: process() takes &mut state because it records phase reads and tool calls
            let gate_output = hooks::phase_gate::process(&input, &mut state, &workflows);
            output.merge(&gate_output);

            // If phase gate blocked, record it
            if gate_output.blocked == Some(true) {
                state.record_blocked();
            }

            // Git hygiene — check uncommitted changes (only for Edit/Write)
            if matches!(input.tool_name.as_deref(), Some("Edit" | "Write")) {
                let hygiene_output = hooks::git_hygiene::process(&input, &git);
                output.merge(&hygiene_output);
            }
        }

        HookEvent::PostToolUse => {
            // Evidence collector runs but needs a phase collection state
            // which only exists during active phase execution
            // For now, just pass through
        }

        HookEvent::Stop => {
            // Context monitor
            let ctx_output = hooks::context_monitor::process(&input);
            output.merge(&ctx_output);

            // Commit hygiene
            let hygiene_output = hooks::commit_hygiene::process(&input, &git);
            output.merge(&hygiene_output);
        }

        HookEvent::SessionStart => {
            // Initialize fresh state
            state = SessionState::new(session_id);
        }

        HookEvent::PreCompact => {
            // Preserve context — no-op for now
        }
    }

    // Record hook invocation
    state.record_hook_invocation(event, 0);

    // Save state AFTER all processing (so phase reads and tool calls are persisted)
    let _ = sentinel_infrastructure::state_store::save(&state);

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
