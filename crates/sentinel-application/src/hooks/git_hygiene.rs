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

/// Returns true when the repo at `repo_dir` is mid-merge / mid-rebase /
/// mid-cherry-pick / mid-revert. Detected by the presence of git's standard
/// sentinel files in the gitdir:
///   - `MERGE_HEAD`         — `git merge` with conflicts
///   - `CHERRY_PICK_HEAD`   — `git cherry-pick` with conflicts
///   - `REVERT_HEAD`        — `git revert` with conflicts
///   - `rebase-merge/`      — interactive / merge-strategy rebase
///   - `rebase-apply/`      — `git am` / classic rebase
///
/// Worktree-aware: when `.git` is a file (`gitdir: <abs path>`), the real
/// gitdir is followed so the sentinel-file lookups land in the right place.
fn is_merge_in_progress(fs: &dyn super::FileSystemPort, repo_dir: &str, git: &dyn GitStatusPort) -> bool {
    let Some(root) = git.repo_root(repo_dir) else {
        return false;
    };
    let dot_git = std::path::Path::new(&root).join(".git");
    let gitdir = if dot_git.is_file() {
        match fs.read_to_string(&dot_git) {
            Ok(content) => match content.lines().find_map(|l| l.strip_prefix("gitdir:")) {
                Some(path) => std::path::PathBuf::from(path.trim()),
                None => return false,
            },
            Err(_) => return false,
        }
    } else {
        dot_git
    };

    fs.exists(&gitdir.join("MERGE_HEAD"))
        || fs.exists(&gitdir.join("CHERRY_PICK_HEAD"))
        || fs.exists(&gitdir.join("REVERT_HEAD"))
        || fs.is_dir(&gitdir.join("rebase-merge"))
        || fs.is_dir(&gitdir.join("rebase-apply"))
}

/// Check whether `file_path` lives inside the git repo rooted at `cwd`.
///
/// Returns `true` if we can't determine a repo root (be conservative —
/// hook continues to apply). Returns `false` only when we have a repo root
/// and the file is clearly outside it.
fn is_path_inside_repo(fs: &dyn super::FileSystemPort, file_path: &str, cwd: &str, git: &dyn GitStatusPort) -> bool {
    let Some(repo_root) = git.repo_root(cwd) else {
        return true;
    };
    let root = std::path::Path::new(&repo_root);
    let target = std::path::Path::new(file_path);
    // Canonicalize best-effort; fall back to raw paths on error.
    let canon_root = fs.canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let canon_target = fs.canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
    canon_target.starts_with(&canon_root)
}

/// Process a git-hygiene hook event (PreToolUse for Edit/Write).
///
/// Checks:
/// 1. Warn if editing directly on main/master (not in a worktree)
/// 2. Block if too many uncommitted files
pub fn process(input: &HookInput, git: &dyn GitStatusPort, fs: &dyn super::FileSystemPort) -> HookOutput {
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
        if !is_path_inside_repo(fs, fp, cwd, git) {
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
    //
    // `git.repo_root` shells out to `git -C <dir> rev-parse` — it needs
    // a *directory*. Pass the file's parent dir, not the file itself
    // (passing a file path silently fails on Windows because
    // `Command::current_dir` on a non-directory aborts the spawn).
    let effective_repo = file_path
        .as_deref()
        .map(|fp| {
            std::path::Path::new(fp)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or(fp)
                .to_string()
        })
        .and_then(|dir| git.repo_root(&dir))
        .unwrap_or_else(|| cwd.to_string());
    let effective_repo_str = effective_repo.as_str();

    // Check 1: BLOCK if on a protected branch and not in a worktree.
    //
    // Exception: when a merge / rebase / cherry-pick / revert is in progress,
    // the user is *resolving conflicts*, not bypassing branch hygiene. Forcing
    // a worktree dance mid-conflict-resolution can drop conflict markers into
    // the merge commit (this happened in the build_notify-ntfy merge — see
    // commit 4fc2f35 for the cleanup).
    if let Ok(branch) = git.current_branch(effective_repo_str) {
        if PROTECTED_BRANCHES.contains(&branch.as_str())
            && !git.is_worktree(effective_repo_str)
            && !is_merge_in_progress(fs, effective_repo_str, git)
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
        fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { None }
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
        fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { None }
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
            process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none(),
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
            process(&input, &git, &crate::hooks::test_support::StubFs).blocked,
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
        assert!(process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_tool_name() {
        let git = StubGit::default_with(false, vec![]);
        assert!(process(&HookInput::default(), &git, &crate::hooks::test_support::StubFs).blocked.is_none());
    }

    #[test]
    fn test_allows_edit_under_threshold() {
        let git = StubGit::default_with(true, vec!["a.rs".into(), "b.rs".into()]);
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none());
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
        let output = process(&input, &git, &crate::hooks::test_support::StubFs);
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
        assert_eq!(process(&input, &git, &crate::hooks::test_support::StubFs).blocked, Some(true));
    }

    #[test]
    fn test_allows_when_no_uncommitted_changes() {
        let git = StubGit::default_with(false, vec![]);
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none());
    }

    #[test]
    fn test_blocks_on_main_without_worktree() {
        let git = StubGit::on_main();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input, &git, &crate::hooks::test_support::StubFs);
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
        let output = process(&input, &git, &crate::hooks::test_support::StubFs);
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
        assert!(process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none());
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
            process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none(),
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
            process(&input, &git, &crate::hooks::test_support::StubFs).blocked,
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
            process(&input, &git, &crate::hooks::test_support::StubFs).blocked.is_none(),
            "should pull file_path from tool_input when input.file_path is empty"
        );
    }

    #[test]
    fn test_protected_branches() {
        assert!(PROTECTED_BRANCHES.contains(&"main"));
        assert!(PROTECTED_BRANCHES.contains(&"master"));
        assert!(!PROTECTED_BRANCHES.contains(&"feat/my-feature"));
    }

    /// Stub that points `repo_root` at a real on-disk tempdir so
    /// `is_merge_in_progress` can read its `.git` dir.
    struct DiskRepoStub {
        repo_root: String,
        branch: String,
        worktree: bool,
    }

    impl GitStatusPort for DiskRepoStub {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> { Ok(vec![]) }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> { Ok(self.branch.clone()) }
        fn is_worktree(&self, _: &str) -> bool { self.worktree }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
        fn repo_root(&self, _: &str) -> Option<String> { Some(self.repo_root.clone()) }
        fn list_worktree_names(&self, _: &str) -> Vec<String> { Vec::new() }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { None }
    }

    fn make_repo_with(sentinel_files: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        for name in sentinel_files {
            // Names ending in `/` are directories (rebase-merge, rebase-apply).
            if let Some(dir) = name.strip_suffix('/') {
                std::fs::create_dir_all(git_dir.join(dir)).unwrap();
            } else {
                std::fs::write(git_dir.join(name), "ref\n").unwrap();
            }
        }
        tmp
    }

    /// Bare repo with no merge-in-progress sentinels — gate should still BLOCK
    /// Edit-on-main (existing behaviour preserved).
    #[test]
    fn test_main_block_still_fires_when_not_merging() {
        let tmp = make_repo_with(&[]);
        let git = DiskRepoStub {
            repo_root: tmp.path().to_string_lossy().into_owned(),
            branch: "main".to_string(),
            worktree: false,
        };
        let target = tmp.path().join("CHANGELOG.md");
        std::fs::write(&target, "x").unwrap();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            cwd: Some(tmp.path().to_string_lossy().into_owned()),
            file_path: Some(target.to_string_lossy().into_owned()),
            ..Default::default()
        };
        assert_eq!(
            process(&input, &git, &crate::hooks::test_support::StubFs).blocked,
            Some(true),
            "edit on main without worktree and without merge-in-progress must still block"
        );
    }

    /// Real-disk `FileSystemPort` impl needed by merge-detection tests
    /// because `is_merge_in_progress` reads MERGE_HEAD / etc. off disk.
    struct RealDiskFs;
    impl super::super::FileSystemPort for RealDiskFs {
        fn home_dir(&self) -> Option<std::path::PathBuf> { dirs::home_dir() }
        fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
        fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> { Ok(vec![]) }
        fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
        fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
        fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

    /// Mid-merge edit on main is now allowed (the new exception). Repeated
    /// for each sentinel file/dir git creates during a conflict.
    #[test]
    fn test_main_edit_allowed_during_active_merge() {
        for sentinels in &[
            &["MERGE_HEAD"][..],
            &["CHERRY_PICK_HEAD"][..],
            &["REVERT_HEAD"][..],
            &["rebase-merge/"][..],
            &["rebase-apply/"][..],
        ] {
            let tmp = make_repo_with(sentinels);
            let git = DiskRepoStub {
                repo_root: tmp.path().to_string_lossy().into_owned(),
                branch: "main".to_string(),
                worktree: false,
            };
            let target = tmp.path().join("CHANGELOG.md");
            std::fs::write(&target, "x").unwrap();
            let input = HookInput {
                tool_name: Some("Edit".to_string()),
                cwd: Some(tmp.path().to_string_lossy().into_owned()),
                file_path: Some(target.to_string_lossy().into_owned()),
                ..Default::default()
            };
            assert!(
                process(&input, &git, &RealDiskFs).blocked.is_none(),
                "edit on main mid-merge should NOT block (sentinel: {sentinels:?})"
            );
        }
    }

    /// `is_merge_in_progress` must follow `.git` files (worktree gitlinks)
    /// so the sentinel-file lookup lands in the real gitdir.
    #[test]
    fn test_merge_detection_follows_gitdir_pointer_files() {
        // Real gitdir holds MERGE_HEAD.
        let real_gitdir_holder = tempfile::tempdir().unwrap();
        let real_gitdir = real_gitdir_holder.path().join("worktrees").join("wt1");
        std::fs::create_dir_all(&real_gitdir).unwrap();
        std::fs::write(real_gitdir.join("MERGE_HEAD"), "ref\n").unwrap();

        // Worktree-style repo: .git is a file pointing at the real gitdir.
        let worktree = tempfile::tempdir().unwrap();
        std::fs::write(
            worktree.path().join(".git"),
            format!("gitdir: {}\n", real_gitdir.display()),
        )
        .unwrap();

        let git = DiskRepoStub {
            repo_root: worktree.path().to_string_lossy().into_owned(),
            branch: "main".to_string(),
            worktree: false,
        };
        struct RealFs;
        impl super::super::FileSystemPort for RealFs {
            fn home_dir(&self) -> Option<std::path::PathBuf> { dirs::home_dir() }
            fn read_to_string(&self, p: &std::path::Path) -> anyhow::Result<String> {
                Ok(std::fs::read_to_string(p)?)
            }
            fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
            fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> { Ok(()) }
            fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> { Ok(vec![]) }
            fn exists(&self, p: &std::path::Path) -> bool { p.exists() }
            fn is_dir(&self, p: &std::path::Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
                Ok(std::fs::metadata(p)?)
            }
            fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }
        assert!(
            is_merge_in_progress(&RealFs, worktree.path().to_str().unwrap(), &git),
            "should follow `.git` gitdir pointer file to the real gitdir"
        );
    }
}
