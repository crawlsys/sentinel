//! PR Merge Gate
//!
//! Warns on `gh pr merge` commands in Bash.
//! The CLAUDE.md says: "Always ask for confirmation before merging a PR.
//! No exceptions."
//!
//! This hook injects a reminder into context so Claude asks the user,
//! but does NOT hard-block the command — the user's approval in the
//! conversation is sufficient (CLAUDE.md enforces the actual rule).
//!
//! Autopilot bypass: if `SENTINEL_AUTOPILOT=1` is set, the ask prompt is
//! downgraded to an `allow` with a context-only reminder, so `gh pr merge`
//! doesn't interrupt the loop with a Yes/No dialog. Gary's CLAUDE.md still
//! requires in-conversation confirmation before hitting merge — this just
//! prevents the harness-level dialog in autopilot mode.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Check if autopilot mode is active via env var.
fn is_autopilot() -> bool {
    std::env::var("SENTINEL_AUTOPILOT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Process a PreToolUse Bash event. Warns on `gh pr merge` but allows it.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    if cmd.contains("gh pr merge") || cmd.contains("gh pr close") {
        if is_autopilot() {
            // Autopilot: inject a reminder via context, do not open the Yes/No dialog.
            return HookOutput::inject_context(
                HookEvent::PreToolUse,
                "[PR Merge Gate] AUTOPILOT: allowing `gh pr merge/close` without a \
                 Yes/No dialog. Verify the in-conversation confirmation was given before \
                 proceeding.",
            );
        }
        return HookOutput::ask(
            "[PR Merge Gate] Claude is attempting to merge/close a PR. Approve to proceed."
        );
    }

    HookOutput::allow()
}

fn extract_bash_command(input: &HookInput) -> Option<&str> {
    input.tool_input.as_ref()?.get("command")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Tests touch SENTINEL_AUTOPILOT env var — serialize them to avoid
    // races with parallel test threads reading stale values.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": cmd})),
            ..Default::default()
        }
    }

    #[test]
    fn test_asks_gh_pr_merge() {
        // Must clear SENTINEL_AUTOPILOT — an inherited `=1` from the caller's
        // shell (e.g. running tests inside a Claude Code autopilot session)
        // would route `process` through the autopilot branch and return
        // inject_context instead of ask.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SENTINEL_AUTOPILOT");
        let out = process(&bash_input("gh pr merge 123"));
        assert!(out.blocked.is_none()); // not hard-blocked
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(sentinel_domain::events::PermissionDecision::Ask));
    }

    #[test]
    fn test_asks_gh_pr_close() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SENTINEL_AUTOPILOT");
        let out = process(&bash_input("gh pr close 42"));
        assert!(out.blocked.is_none());
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(sentinel_domain::events::PermissionDecision::Ask));
    }

    #[test]
    fn test_allows_gh_pr_view() {
        assert!(process(&bash_input("gh pr view 123")).blocked.is_none());
    }

    #[test]
    fn test_allows_gh_pr_create() {
        assert!(process(&bash_input("gh pr create --title test")).blocked.is_none());
    }

    #[test]
    fn test_allows_non_gh_commands() {
        assert!(process(&bash_input("git push")).blocked.is_none());
        assert!(process(&bash_input("cargo test")).blocked.is_none());
    }

    #[test]
    fn test_allows_no_tool_input() {
        assert!(process(&HookInput::default()).blocked.is_none());
    }

    #[test]
    fn test_autopilot_downgrades_ask_to_context_inject() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SENTINEL_AUTOPILOT", "1");
        let out = process(&bash_input("gh pr merge 123 --squash"));
        std::env::remove_var("SENTINEL_AUTOPILOT");

        assert!(out.blocked.is_none());
        let hso = out.hook_specific_output.expect("output should have hso");
        // Should NOT be asking for permission.
        assert_ne!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask),
            "autopilot must not trigger the Yes/No dialog"
        );
        // Should inject a context reminder instead.
        let ctx = hso.additional_context.unwrap_or_default();
        assert!(ctx.contains("AUTOPILOT"), "expected AUTOPILOT reminder in context, got: {ctx}");
    }

    #[test]
    fn test_autopilot_false_still_asks() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SENTINEL_AUTOPILOT", "0");
        let out = process(&bash_input("gh pr merge 7"));
        std::env::remove_var("SENTINEL_AUTOPILOT");

        let hso = out.hook_specific_output.unwrap();
        assert_eq!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask)
        );
    }

    #[test]
    fn test_no_autopilot_env_still_asks() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SENTINEL_AUTOPILOT");
        let out = process(&bash_input("gh pr close 42"));
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(
            hso.permission_decision,
            Some(sentinel_domain::events::PermissionDecision::Ask)
        );
    }
}
