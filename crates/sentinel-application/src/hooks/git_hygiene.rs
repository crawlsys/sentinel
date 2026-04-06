//! Git Hygiene Gate
//!
//! Blocks Edit/Write tools when uncommitted changes exceed thresholds.
//! Warns when editing directly on main/master (should use feature branches).
//! Encourages small, frequent commits via worktree workflow.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::GitStatusPort;

const MAX_UNCOMMITTED_FILES: usize = 10;

/// Protected branch names that should not receive direct edits.
const PROTECTED_BRANCHES: &[&str] = &["main", "master"];

/// Get the current git branch name from the working directory.
fn current_branch(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Check if we're inside a git worktree (not the main working tree).
fn is_worktree(cwd: &str) -> bool {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    if output.is_err() {
        return false;
    }

    // Check if .git is a file (worktree) vs directory (main tree)
    let git_path = std::path::Path::new(cwd).join(".git");
    git_path.is_file() // worktrees have .git as a file, not a directory
}

/// Process a git-hygiene hook event (PreToolUse for Edit/Write).
///
/// Checks:
/// 1. Warn if editing directly on main/master (not in a worktree)
/// 2. Block if too many uncommitted files
pub fn process(input: &HookInput, git: &dyn GitStatusPort) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return HookOutput::allow(),
    };

    // Only gate Edit and Write
    if tool != "Edit" && tool != "Write" {
        return HookOutput::allow();
    }

    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Check 1: Warn if on a protected branch and not in a worktree
    if let Some(branch) = current_branch(cwd) {
        if PROTECTED_BRANCHES.contains(&branch.as_str()) && !is_worktree(cwd) {
            return HookOutput::inject_context(
                HookEvent::PreToolUse,
                format!(
                    "[Git Hygiene] You are editing directly on `{branch}`. \
                     Use `EnterWorktree` to create an isolated branch first. \
                     Direct edits to {branch} should be avoided — use feature branches."
                ),
            );
        }
    }

    // Check 2: Block if too many uncommitted files
    match git.has_uncommitted_changes(cwd) {
        Ok(true) => match git.changed_files(cwd) {
            Ok(files) if files.len() > MAX_UNCOMMITTED_FILES => HookOutput::deny(format!(
                "Git hygiene: {} uncommitted files (threshold: {}). \
                 Commit your changes before making more edits.\n\
                 Changed files: {}",
                files.len(),
                MAX_UNCOMMITTED_FILES,
                files.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            )),
            _ => HookOutput::allow(),
        },
        _ => HookOutput::allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubGit {
        has_changes: bool,
        files: Vec<String>,
    }

    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _repo_path: &str) -> anyhow::Result<bool> {
            Ok(self.has_changes)
        }

        fn changed_files(&self, _repo_path: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.files.clone())
        }
    }

    #[test]
    fn test_allows_non_edit_tools() {
        let git = StubGit {
            has_changes: true,
            files: vec!["a.rs".into(); 20],
        };
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_tool_name() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput::default();
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_edit_under_threshold() {
        let git = StubGit {
            has_changes: true,
            files: vec!["a.rs".into(), "b.rs".into()],
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        // May inject context (branch warning) but should not block
        assert!(output.blocked.is_none() || output.blocked == Some(false));
    }

    #[test]
    fn test_blocks_edit_over_threshold() {
        let files: Vec<String> = (0..15).map(|i| format!("file{i}.rs")).collect();
        let git = StubGit {
            has_changes: true,
            files,
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            // Use a non-git dir so branch check doesn't interfere
            cwd: Some("/tmp/not-a-git-repo".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert_eq!(output.blocked, Some(true));
        let reason = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .or(output.reason.as_deref())
            .unwrap();
        assert!(reason.contains("15 uncommitted files"));
    }

    #[test]
    fn test_blocks_write_over_threshold() {
        let files: Vec<String> = (0..12).map(|i| format!("file{i}.rs")).collect();
        let git = StubGit {
            has_changes: true,
            files,
        };
        let input = HookInput {
            tool_name: Some("Write".to_string()),
            cwd: Some("/tmp/not-a-git-repo".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_allows_when_no_uncommitted_changes() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_protected_branches() {
        assert!(PROTECTED_BRANCHES.contains(&"main"));
        assert!(PROTECTED_BRANCHES.contains(&"master"));
        assert!(!PROTECTED_BRANCHES.contains(&"feat/my-feature"));
    }

    #[test]
    fn test_current_branch_non_git_dir() {
        // Non-git directory should return None
        assert!(current_branch("/tmp").is_none());
    }
}
