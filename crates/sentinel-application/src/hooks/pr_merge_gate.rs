//! PR Merge Gate
//!
//! HARD BLOCK on `gh pr merge` commands in Bash.
//! The CLAUDE.md says: "Always ask for confirmation before merging a PR.
//! No exceptions."
//!
//! This hook ensures Claude cannot merge a PR without the user explicitly
//! approving it first. The block message tells Claude to ask the user.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process a PreToolUse Bash event. Blocks `gh pr merge`.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    if cmd.contains("gh pr merge") || cmd.contains("gh pr close") {
        return HookOutput::deny(
            "[PR Merge Gate] BLOCKED: PR merge/close requires explicit user confirmation. \
             Ask Gary before merging or closing any pull request. No exceptions."
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
    fn test_blocks_gh_pr_merge() {
        assert_eq!(process(&bash_input("gh pr merge 123")).blocked, Some(true));
        assert_eq!(process(&bash_input("gh pr merge --squash")).blocked, Some(true));
    }

    #[test]
    fn test_blocks_gh_pr_close() {
        assert_eq!(process(&bash_input("gh pr close 42")).blocked, Some(true));
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
