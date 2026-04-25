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

/// Extract the target file path from a HookInput.
///
/// Checks `input.file_path` (Claude Code 2.1.89+), then falls back to
/// `tool_input.file_path` for older runtimes.
fn file_path_from_input(input: &HookInput) -> Option<String> {
    if let Some(p) = &input.file_path {
        if !p.is_empty() {
            return Some(p.clone());
        }
    }
    input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

/// Check whether `file_path` lives inside the git repo rooted at `cwd`.
///
/// Returns `true` if we can't determine a repo root (be conservative —
/// hook continues to apply). Returns `false` only when we have a repo root
/// and the file is clearly outside it.
fn is_path_inside_repo(file_path: &str, cwd: &str, git: &dyn GitStatusPort) -> bool {
    let Some(repo_root) = git.repo_root(cwd) else {
        return true;
    };
    let root = std::path::Path::new(&repo_root);
    let target = std::path::Path::new(file_path);
    // Canonicalize best-effort; fall back to raw paths on error.
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon_target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    canon_target.starts_with(&canon_root)
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

    // If the target file is outside the cwd's repo, this hook doesn't apply.
    // git_hygiene governs the *current* repo — edits to files outside it
    // (e.g. ~/.claude/ config files) are always allowed.
    let file_path = file_path_from_input(input);
    if let Some(fp) = file_path.as_deref() {
        if !is_path_inside_repo(fp, cwd, git) {
            return HookOutput::allow();
        }
    }

    // Resolve the effective repo path for branch/worktree checks.
    //
    // When the session cwd is the primary repo checkout (on main) but the
    // edit targets a file inside `.claude/worktrees/*` or `.worktrees/*`,
    // using `cwd` here reports "main" even though the file actually lives
    // on a feature branch in a worktree. That falsely blocks legitimate
    // worktree edits. Resolve the target file's own repo root instead and
    // fall back to `cwd` only when the file path is absent or outside any
    // repo.
    let effective_repo = file_path
        .as_deref()
        .and_then(|fp| git.repo_root(fp))
        .unwrap_or_else(|| cwd.to_string());
    let effective_repo_str = effective_repo.as_str();

    // Check 1: BLOCK if on a protected branch and not in a worktree
    if let Ok(branch) = git.current_branch(effective_repo_str) {
        if PROTECTED_BRANCHES.contains(&branch.as_str())
            && !git.is_worktree(effective_repo_str)
        {
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
        repo_root: Option<String>,
    }

    impl StubGit {
        fn default_with(has_changes: bool, files: Vec<String>) -> Self {
            Self {
                has_changes,
                files,
                branch: "feat/test".to_string(),
                worktree: false,
                repo_root: Some("/repo".to_string()),
            }
        }

        fn on_main() -> Self {
            Self {
                has_changes: false,
                files: vec![],
                branch: "main".to_string(),
                worktree: false,
                repo_root: Some("/repo".to_string()),
            }
        }

        fn on_main_worktree() -> Self {
            Self {
                has_changes: false,
                files: vec![],
                branch: "main".to_string(),
                worktree: true,
                repo_root: Some("/repo".to_string()),
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
        fn repo_root(&self, _path: &str) -> Option<String> {
            self.repo_root.clone()
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
    }

    /// Stub that returns a different branch (+ worktree status + repo_root)
    /// depending on the path. Used to regression-test that git_hygiene
    /// resolves the branch from the *target file's* repo root rather than
    /// from the session cwd.
    struct PathAwareStubGit {
        primary_root: String,
        worktree_root: String,
    }

    impl GitStatusPort for PathAwareStubGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn current_branch(&self, repo_path: &str) -> anyhow::Result<String> {
            if repo_path == self.worktree_root {
                Ok("feat/wt".to_string())
            } else {
                Ok("main".to_string())
            }
        }
        fn is_worktree(&self, repo_path: &str) -> bool {
            repo_path == self.worktree_root
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn repo_root(&self, path: &str) -> Option<String> {
            if path.starts_with(&self.worktree_root) {
                Some(self.worktree_root.clone())
            } else if path.starts_with(&self.primary_root) {
                Some(self.primary_root.clone())
            } else {
                None
            }
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
    }

    /// Regression: session cwd is the primary repo on main, but the edit
    /// targets a file inside a worktree on a feature branch. Must NOT
    /// block — the file's own repo root is a worktree off feat/wt.
    #[test]
    fn test_worktree_edit_from_main_cwd_not_blocked() {
        let git = PathAwareStubGit {
            primary_root: "/repo".to_string(),
            worktree_root: "/repo/.worktrees/feature".to_string(),
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some("/repo".to_string()),
            file_path: Some("/repo/.worktrees/feature/src/lib.rs".to_string()),
            ..Default::default()
        };
        assert!(
            process(&input, &git).blocked.is_none(),
            "worktree edits from main cwd should not be blocked"
        );
    }

    /// Regression: cwd is on main, file path is inside the primary repo
    /// on main (no worktree) — must still block (previous behaviour).
    #[test]
    fn test_direct_main_edit_still_blocked() {
        let git = PathAwareStubGit {
            primary_root: "/repo".to_string(),
            worktree_root: "/repo/.worktrees/feature".to_string(),
        };
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some("/repo".to_string()),
            file_path: Some("/repo/src/main.rs".to_string()),
            ..Default::default()
        };
        assert_eq!(
            process(&input, &git).blocked,
            Some(true),
            "direct main edits should still be blocked"
        );
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
    fn test_allows_edit_to_file_outside_repo() {
        // cwd is the sentinel repo on main, but target file is ~/.claude/foo.json
        // — not inside this repo, so the hook must allow it.
        let git = StubGit::on_main();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some("/repo".to_string()),
            file_path: Some("/home/user/.claude/config.json".to_string()),
            ..Default::default()
        };
        assert!(
            process(&input, &git).blocked.is_none(),
            "should allow edits to files outside the cwd's repo"
        );
    }

    #[test]
    fn test_blocks_edit_to_file_inside_repo_on_main() {
        let git = StubGit::on_main();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some("/repo".to_string()),
            file_path: Some("/repo/src/foo.rs".to_string()),
            ..Default::default()
        };
        assert_eq!(
            process(&input, &git).blocked,
            Some(true),
            "should still block edits inside the repo on main"
        );
    }

    #[test]
    fn test_file_path_from_tool_input_fallback() {
        let git = StubGit::on_main();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some("/repo".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "/home/user/outside.txt"
            })),
            ..Default::default()
        };
        assert!(
            process(&input, &git).blocked.is_none(),
            "should pull file_path from tool_input when input.file_path is empty"
        );
    }

    #[test]
    fn test_protected_branches() {
        assert!(PROTECTED_BRANCHES.contains(&"main"));
        assert!(PROTECTED_BRANCHES.contains(&"master"));
        assert!(!PROTECTED_BRANCHES.contains(&"feat/my-feature"));
    }
}
