//! Git Hygiene Gate
//!
//! Blocks Edit/Write tools when uncommitted changes exceed thresholds.
//! Hard-blocks editing on main/master without a worktree.
//! All git operations go through the injected GitStatusPort — no direct
//! std::process::Command calls.

use sentinel_domain::constants;
use sentinel_domain::events::{HookInput, HookOutput};

use super::GitStatusPort;

const MAX_UNCOMMITTED_FILES: usize = constants::MAX_UNCOMMITTED_FILES;

/// Protected branch names that should not receive direct edits.
const PROTECTED_BRANCHES: &[&str] = &["main", "master"];

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

    // Check 1: BLOCK if on a protected branch and not in a worktree
    if let Ok(branch) = git.current_branch(cwd) {
        if PROTECTED_BRANCHES.contains(&branch.as_str()) && !git.is_worktree(cwd) {
            return HookOutput::deny(format!(
                "[Git Hygiene] BLOCKED: editing directly on `{branch}` without a worktree. \
                 Use `EnterWorktree` to create an isolated branch first. \
                 Direct edits to protected branches are not allowed."
            ));
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
        branch: String,
        worktree: bool,
    }

    impl StubGit {
        fn default_with(has_changes: bool, files: Vec<String>) -> Self {
            Self {
                has_changes,
                files,
                branch: "feat/test".to_string(),
                worktree: false,
            }
        }

        fn on_main() -> Self {
            Self {
                has_changes: false,
                files: vec![],
                branch: "main".to_string(),
                worktree: false,
            }
        }

        fn on_main_worktree() -> Self {
            Self {
                has_changes: false,
                files: vec![],
                branch: "main".to_string(),
                worktree: true,
            }
        }
    }

    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _repo_path: &str) -> anyhow::Result<bool> {
            Ok(self.has_changes)
        }
        fn changed_files(&self, _repo_path: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.files.clone())
        }
        fn current_branch(&self, _repo_path: &str) -> anyhow::Result<String> {
            Ok(self.branch.clone())
        }
        fn is_worktree(&self, _repo_path: &str) -> bool {
            self.worktree
        }
        fn has_unpushed_commits(&self, _repo_path: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
    }

    #[test]
    fn test_allows_non_edit_tools() {
        let git = StubGit::default_with(true, vec!["a.rs".into(); 20]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &git).blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_tool_name() {
        let git = StubGit::default_with(false, vec![]);
        assert!(process(&HookInput::default(), &git).blocked.is_none());
    }

    #[test]
    fn test_allows_edit_under_threshold() {
        let git = StubGit::default_with(true, vec!["a.rs".into(), "b.rs".into()]);
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &git).blocked.is_none());
    }

    #[test]
    fn test_blocks_edit_over_threshold() {
        let files: Vec<String> = (0..15).map(|i| format!("file{i}.rs")).collect();
        let git = StubGit::default_with(true, files);
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_write_over_threshold() {
        let files: Vec<String> = (0..12).map(|i| format!("file{i}.rs")).collect();
        let git = StubGit::default_with(true, files);
        let input = HookInput {
            tool_name: Some("Write".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert_eq!(process(&input, &git).blocked, Some(true));
    }

    #[test]
    fn test_allows_when_no_uncommitted_changes() {
        let git = StubGit::default_with(false, vec![]);
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &git).blocked.is_none());
    }

    #[test]
    fn test_blocks_on_main_without_worktree() {
        let git = StubGit::on_main();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert_eq!(output.blocked, Some(true), "Should hard-block edits on main without worktree");
    }

    #[test]
    fn test_allows_main_in_worktree() {
        let git = StubGit::on_main_worktree();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git);
        assert!(output.blocked.is_none());
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.is_none(), "Should NOT warn in worktree");
    }

    #[test]
    fn test_allows_feature_branch() {
        let git = StubGit::default_with(false, vec![]);
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &git).blocked.is_none());
    }

    #[test]
    fn test_protected_branches() {
        assert!(PROTECTED_BRANCHES.contains(&"main"));
        assert!(PROTECTED_BRANCHES.contains(&"master"));
        assert!(!PROTECTED_BRANCHES.contains(&"feat/my-feature"));
    }
}
