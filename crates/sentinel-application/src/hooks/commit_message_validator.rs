//! Commit Message Validator — PreToolUse hook
//!
//! Validates that `git commit` commands use conventional commit format
//! (feat:, fix:, chore:, docs:, refactor:, test:, style:, perf:, ci:, build:).
//! Fires on PreToolUse for Bash tool calls containing `git commit`.
//!
//! Does NOT block — injects a warning message so Claude can fix the message
//! before proceeding. Only blocks on truly malformed messages.

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};

/// Conventional commit prefixes (lowercase)
const VALID_PREFIXES: &[&str] = &[
    "feat", "fix", "chore", "docs", "refactor", "test", "style", "perf", "ci", "build", "revert",
];

/// Extract the commit message from a git commit command.
/// Handles: -m "msg", -m 'msg', -m "$(cat <<'EOF'\nmsg\nEOF\n)"
fn extract_commit_message(command: &str) -> Option<String> {
    // Pattern 1: heredoc style — git commit -m "$(cat <<'EOF'\n...\nEOF\n)"
    // Extract the first line after EOF marker as the message
    let heredoc_re = Regex::new(r#"<<'?EOF'?\s*\n(.+)"#).ok()?;
    if let Some(caps) = heredoc_re.captures(command) {
        return Some(caps[1].trim().to_string());
    }

    // Pattern 2: -m "message" or -m 'message'
    let quoted_re = Regex::new(r#"-m\s+["']([^"']+)["']"#).ok()?;
    if let Some(caps) = quoted_re.captures(command) {
        return Some(caps[1].trim().to_string());
    }

    // Pattern 3: -m message (unquoted, single word)
    let unquoted_re = Regex::new(r#"-m\s+(\S+)"#).ok()?;
    if let Some(caps) = unquoted_re.captures(command) {
        return Some(caps[1].trim().to_string());
    }

    None
}

/// Check if a commit message follows conventional commit format.
fn is_conventional(message: &str) -> bool {
    // Get the first line (subject)
    let subject = message.lines().next().unwrap_or(message).trim();

    if subject.is_empty() {
        return false;
    }

    // Check for type prefix: "type:" or "type(scope):"
    let prefix_re = match Regex::new(r"^(\w+)(?:\([^)]*\))?:\s*.+") {
        Ok(re) => re,
        Err(_) => return false,
    };

    let caps = match prefix_re.captures(subject) {
        Some(c) => c,
        None => return false,
    };

    let prefix = caps[1].to_lowercase();
    VALID_PREFIXES.contains(&prefix.as_str())
}

/// Process PreToolUse for Bash commands containing git commit.
pub fn process(input: &HookInput) -> HookOutput {
    // Only act on Bash tool calls
    if input.tool_name.as_deref() != Some("Bash") {
        return HookOutput::allow();
    }

    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    // Only check git commit (not push, not other git commands)
    let commit_re = match Regex::new(r"\bgit\s+commit\b") {
        Ok(re) => re,
        Err(_) => return HookOutput::allow(),
    };

    if !commit_re.is_match(command) {
        return HookOutput::allow();
    }

    // Skip --amend (often fixups, less important to validate)
    if command.contains("--amend") {
        return HookOutput::allow();
    }

    // Extract the commit message
    let message = match extract_commit_message(command) {
        Some(m) => m,
        // No -m flag — might be opening editor, allow
        None => return HookOutput::allow(),
    };

    if is_conventional(&message) {
        return HookOutput::allow();
    }

    // Not conventional — block with guidance
    let valid_list = VALID_PREFIXES
        .iter()
        .map(|p| format!("`{p}:`"))
        .collect::<Vec<_>>()
        .join(", ");

    let reason = format!(
        "Commit message doesn't follow conventional format.\n\
         Got: \"{message}\"\n\
         Expected: <type>(<scope>): <description>\n\
         Valid types: {valid_list}\n\
         Examples: \"feat: add user auth\", \"fix(api): handle null response\", \"chore: bump deps\""
    );

    HookOutput::block(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_allows_non_bash() {
        let input = HookInput {
            tool_name: Some("Read".into()),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_non_git_command() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "cargo test"})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_conventional_feat() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"feat: add user auth\""})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_conventional_fix_with_scope() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(
                json!({"command": "git commit -m \"fix(api): handle null response\""}),
            ),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_conventional_chore() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m 'chore: bump deps'"})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_blocks_non_conventional() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"updated the thing\""})),
            ..Default::default()
        };
        let output = process(&input);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("conventional"));
    }

    #[test]
    fn test_blocks_no_prefix() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"add new feature\""})),
            ..Default::default()
        };
        let output = process(&input);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_invalid_prefix() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit -m \"update: changed something\""})),
            ..Default::default()
        };
        let output = process(&input);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_allows_amend() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit --amend -m \"whatever\""})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_no_message_flag() {
        // git commit without -m (opens editor)
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git commit"})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_heredoc_conventional() {
        let cmd = "git commit -m \"$(cat <<'EOF'\nfeat: add hooks engine\n\nCo-Authored-By: Claude\nEOF\n)\"";
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": cmd})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_extract_message_double_quotes() {
        assert_eq!(
            extract_commit_message("git commit -m \"feat: test\""),
            Some("feat: test".into())
        );
    }

    #[test]
    fn test_extract_message_single_quotes() {
        assert_eq!(
            extract_commit_message("git commit -m 'fix: bug'"),
            Some("fix: bug".into())
        );
    }

    #[test]
    fn test_extract_message_heredoc() {
        let cmd = "git commit -m \"$(cat <<'EOF'\nchore: bump version\n\nBody here\nEOF\n)\"";
        assert_eq!(
            extract_commit_message(cmd),
            Some("chore: bump version".into())
        );
    }

    #[test]
    fn test_is_conventional_all_types() {
        for prefix in VALID_PREFIXES {
            let msg = format!("{prefix}: some change");
            assert!(is_conventional(&msg), "Expected '{msg}' to be conventional");
        }
    }

    #[test]
    fn test_is_conventional_with_scope() {
        assert!(is_conventional("feat(hooks): add pre-compact"));
        assert!(is_conventional("fix(dashboard): layout issue"));
    }

    #[test]
    fn test_not_conventional() {
        assert!(!is_conventional("updated stuff"));
        assert!(!is_conventional("WIP"));
        assert!(!is_conventional(""));
        assert!(!is_conventional("random: not a valid type"));
    }

    #[test]
    fn test_allows_git_push() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "git push origin main"})),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }

    #[test]
    fn test_allows_no_tool_input() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            ..Default::default()
        };
        assert!(process(&input).blocked.is_none());
    }
}
