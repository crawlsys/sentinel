//! PR Merge Gate
//!
//! Warns on `gh pr merge` commands in Bash.
//! The CLAUDE.md says: "Always ask for confirmation before merging a PR.
//! No exceptions."
//!
//! This hook injects a reminder into context so Claude asks the user,
//! but does NOT hard-block the command — the user's approval in the
//! conversation is sufficient (CLAUDE.md enforces the actual rule).

use sentinel_domain::events::{HookInput, HookOutput};

/// Process a PreToolUse Bash event. Warns on `gh pr merge` but allows it.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    if cmd.contains("gh pr merge") || cmd.contains("gh pr close") {
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

    fn bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": cmd})),
            ..Default::default()
        }
    }

    #[test]
    fn test_asks_gh_pr_merge() {
        let out = process(&bash_input("gh pr merge 123"));
        assert!(out.blocked.is_none()); // not hard-blocked
        let hso = out.hook_specific_output.unwrap();
        assert_eq!(hso.permission_decision, Some(sentinel_domain::events::PermissionDecision::Ask));
    }

    #[test]
    fn test_asks_gh_pr_close() {
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
}
