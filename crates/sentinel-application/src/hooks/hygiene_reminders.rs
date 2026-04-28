//! Hygiene Reminders — Two-phase hook
//!
//! **Stop phase:** Detects conditions that need reminders:
//!   1. Unpushed commits (local ahead of remote)
//!   2. Stale worktrees (merged but not cleaned up) — scoped to current repo
//!   3. Missing changelog updates (code changed but CHANGELOG.md not updated)
//!
//! State is scoped by repo root to prevent cross-project bleeding.
//! State file: `~/.claude/sentinel/state/hygiene-{repo_hash}.json`
//!
//! **UserPromptSubmit phase:** Reads state and injects reminders.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ReminderState {
    #[serde(default)]
    unpushed_commits: bool,
    #[serde(default)]
    stale_worktrees: Vec<String>,
    #[serde(default)]
    changelog_stale: bool,
    /// Local branches merged into main and named `worktree-*` — safe to delete.
    #[serde(default)]
    merged_local_worktree_branches: Vec<String>,
    /// Remote branches merged into main and named `worktree-*` — safe to push-delete.
    #[serde(default)]
    merged_remote_worktree_branches: Vec<String>,
    /// The repo root this state was computed for — sanity check on load.
    #[serde(default)]
    repo_root: String,
}

/// Derive a short stable hash from the repo root path for use in the filename.
/// Uses a simple djb2-style hash — no crypto needed, just stable across runs.
fn repo_hash(repo_root: &str) -> String {
    let mut h: u64 = 5381;
    for b in repo_root.bytes() {
        h = h.wrapping_mul(33).wrapping_add(u64(b));
    }
    format!("{h:016x}")
}

fn u64(b: u8) -> u64 {
    b as u64
}

fn state_file(fs: &dyn FileSystemPort, repo_root: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = home.join(".claude").join("sentinel").join("state");
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join(format!("hygiene-{}.json", repo_hash(repo_root))))
}

// ── Stop phase: detect conditions ──────────────────────────────────────

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Scope everything to the repo root — prevents cross-project state bleeding.
    let repo_root = match ctx.git.repo_root(cwd) {
        Some(r) => r,
        None => return HookOutput::allow(), // not a git repo
    };

    let mut state = ReminderState {
        repo_root: repo_root.clone(),
        ..Default::default()
    };

    // 1. Check for unpushed commits
    if let Ok(has_unpushed) = ctx.git.has_unpushed_commits(cwd) {
        state.unpushed_commits = has_unpushed;
    }

    // 2. Stale worktrees — dirs inside this repo's `.claude/worktrees/` that
    //    are NOT registered in `git worktree list`. A registered worktree is
    //    actively in use (possibly by a parallel agent session) — flagging it
    //    as stale produces the repeating "N stale worktrees" noise that
    //    never clears on its own. True staleness is: directory present on
    //    disk, absent from git's registry (orphaned by a failed removal or
    //    aborted session).
    //
    //    `list_worktree_names()` returns the basename of every registered
    //    worktree path. If the git call fails (empty Vec), we skip the
    //    staleness check entirely — false-positiving every real worktree
    //    is worse than silently skipping once.
    let worktree_dir = PathBuf::from(&repo_root).join(".claude").join("worktrees");
    if ctx.fs.is_dir(&worktree_dir) {
        let registered: std::collections::HashSet<String> = ctx
            .git
            .list_worktree_names(&repo_root)
            .into_iter()
            .collect();

        if !registered.is_empty() {
            if let Ok(entries) = ctx.fs.read_dir(&worktree_dir) {
                for entry in entries {
                    if ctx.fs.is_dir(&entry) {
                        if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
                            if !registered.contains(name) {
                                state.stale_worktrees.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // 3a. Merged worktree-* branches (local + remote) — orphaned branches that
    //     are fully merged into main. Surfaces the cleanup commands so Gary
    //     can prune them without manually running `git branch --merged`.
    state.merged_local_worktree_branches = ctx
        .git
        .merged_local_branches(&repo_root, "main")
        .into_iter()
        .filter(|b| b.starts_with("worktree-"))
        .collect();
    state.merged_remote_worktree_branches = ctx
        .git
        .merged_remote_branches(&repo_root, "main")
        .into_iter()
        .filter(|b| b.starts_with("worktree-"))
        .collect();

    // 4. Changelog staleness — code files changed but CHANGELOG.md not touched.
    //    Uses uncommitted changed files (git status). After a commit this clears
    //    naturally since changed_files returns empty. That's correct behaviour:
    //    if you committed without updating CHANGELOG, doc_drift catches it instead.
    let changelog_path = PathBuf::from(&repo_root).join("CHANGELOG.md");
    if ctx.fs.exists(&changelog_path) {
        if let Ok(files) = ctx.git.changed_files(cwd) {
            let has_code_changes = files
                .iter()
                .any(|f| sentinel_domain::file_kind::is_code_file(f));
            let changelog_changed = files.iter().any(|f| f.contains("CHANGELOG"));
            state.changelog_stale = has_code_changes && !changelog_changed;
        }
    }

    // Write state scoped to this repo root
    if let Some(path) = state_file(ctx.fs, &repo_root) {
        if let Ok(json) = serde_json::to_string(&state) {
            let _ = ctx.fs.write(&path, json.as_bytes());
        }
    }

    HookOutput::allow()
}

// `is_code_file` has moved to `sentinel_domain::file_kind`.
// The hook calls it inline above (single call site).

// ── UserPromptSubmit phase: inject reminders ───────────────────────────

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Load state scoped to the current repo root only.
    let repo_root = match ctx.git.repo_root(cwd) {
        Some(r) => r,
        None => return HookOutput::allow(),
    };

    let path = match state_file(ctx.fs, &repo_root) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: ReminderState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Sanity check: state must belong to this repo
    if state.repo_root != repo_root {
        return HookOutput::allow();
    }

    let mut reminders = Vec::new();

    if state.unpushed_commits {
        reminders.push(
            "[Push Reminder] You have unpushed commits on the current branch. \
             Push to remote after merging: `git push`."
                .to_string(),
        );
    }

    // Re-validate stale worktrees at read time. The state file is written by
    // the Stop-phase `process()` and read by every UserPromptSubmit; without
    // this filter, a stale entry persists across many prompts even after the
    // user (or `ExitWorktree`) has removed the directory, until Stop fires
    // again. Validating against the live filesystem here makes the hook self-
    // healing: the reminder disappears the next prompt after cleanup.
    let worktree_dir = PathBuf::from(&state.repo_root).join(".claude").join("worktrees");
    let still_stale: Vec<String> = state
        .stale_worktrees
        .iter()
        .filter(|name| ctx.fs.is_dir(&worktree_dir.join(name)))
        .cloned()
        .collect();

    if !still_stale.is_empty() {
        reminders.push(format!(
            "[Worktree Cleanup] {} stale worktree(s) found: {}. \
             Clean up with `ExitWorktree(action: \"remove\")` or `git worktree remove`.",
            still_stale.len(),
            still_stale.join(", ")
        ));
    }

    if !state.merged_local_worktree_branches.is_empty() {
        let cmds = state
            .merged_local_worktree_branches
            .iter()
            .map(|b| format!("  git branch -d {b}"))
            .collect::<Vec<_>>()
            .join("\n");
        reminders.push(format!(
            "[Branch Cleanup] {} local `worktree-*` branch(es) merged into main \
             — safe to delete:\n{cmds}",
            state.merged_local_worktree_branches.len()
        ));
    }

    if !state.merged_remote_worktree_branches.is_empty() {
        let cmds = state
            .merged_remote_worktree_branches
            .iter()
            .map(|b| format!("  git push origin --delete {b}"))
            .collect::<Vec<_>>()
            .join("\n");
        reminders.push(format!(
            "[Remote Branch Cleanup] {} remote `worktree-*` branch(es) merged into main \
             — safe to delete:\n{cmds}",
            state.merged_remote_worktree_branches.len()
        ));
    }

    if state.changelog_stale {
        reminders.push(
            "[Changelog] Code files were changed but CHANGELOG.md was not updated. \
             Add an entry under `## [Unreleased]`."
                .to_string(),
        );
    }

    if reminders.is_empty() {
        return HookOutput::allow();
    }

    HookOutput::inject_context(HookEvent::UserPromptSubmit, reminders.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_state_produces_no_reminders() {
        let state = ReminderState::default();
        assert!(!state.unpushed_commits);
        assert!(state.stale_worktrees.is_empty());
        assert!(!state.changelog_stale);
    }

    #[test]
    fn test_state_round_trips() {
        let state = ReminderState {
            unpushed_commits: true,
            stale_worktrees: vec!["feat+old".to_string()],
            changelog_stale: true,
            merged_local_worktree_branches: vec!["worktree-feat+x".to_string()],
            merged_remote_worktree_branches: vec!["worktree-fix+y".to_string()],
            repo_root: "/repos/sentinel".to_string(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ReminderState = serde_json::from_str(&json).unwrap();
        assert!(parsed.unpushed_commits);
        assert_eq!(parsed.stale_worktrees.len(), 1);
        assert!(parsed.changelog_stale);
        assert_eq!(parsed.merged_local_worktree_branches, vec!["worktree-feat+x"]);
        assert_eq!(parsed.merged_remote_worktree_branches, vec!["worktree-fix+y"]);
        assert_eq!(parsed.repo_root, "/repos/sentinel");
    }

    #[test]
    fn test_repo_hash_is_stable() {
        let h1 = repo_hash("/repos/sentinel");
        let h2 = repo_hash("/repos/sentinel");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_repo_hash_differs_for_different_roots() {
        let h1 = repo_hash("/repos/sentinel");
        let h2 = repo_hash("/repos/other-project");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_state_file_differs_per_repo() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let path1 = state_file(ctx.fs, "/repos/sentinel");
        let path2 = state_file(ctx.fs, "/repos/other-project");
        assert_ne!(path1, path2);
    }

    #[test]
    fn test_repo_root_mismatch_produces_no_reminders() {
        // State written for repo A should not inject reminders when in repo B
        let state = ReminderState {
            unpushed_commits: true,
            stale_worktrees: vec!["old-branch".to_string()],
            changelog_stale: true,
            repo_root: "/repos/sentinel".to_string(),
            ..Default::default()
        };
        // Simulate: state says sentinel, but current repo_root is "other-project"
        assert_ne!(state.repo_root, "/repos/other-project");
    }

    // is_code_file tests live in `sentinel_domain::file_kind::tests`.

    /// Regression: process_prompt must drop stale_worktrees entries whose
    /// directory no longer exists on disk. Before the fix, the hook kept
    /// re-injecting the reminder on every prompt until the next Stop reran
    /// `process()` — this could persist across many turns after the user
    /// (or `ExitWorktree`) had already cleaned up.
    #[test]
    fn test_stale_worktrees_filtered_when_dir_removed() {
        use crate::hooks::FileSystemPort;
        use std::path::{Path, PathBuf};

        // FS stub that reports `is_dir(...)` = true for `/repo/.claude/worktrees`
        // (the parent dir, so the reminder code path is reachable) but false
        // for the `/repo/.claude/worktrees/<name>` child the state names —
        // i.e. the worktree was removed since state was last written.
        struct DirGoneFs;
        impl FileSystemPort for DirGoneFs {
            fn home_dir(&self) -> Option<PathBuf> { Some(PathBuf::from("/mock/home")) }
            fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
                // Inject the cached state file the hook reads on UserPromptSubmit.
                let state = ReminderState {
                    repo_root: "/repo".to_string(),
                    stale_worktrees: vec!["already-removed".to_string()],
                    ..Default::default()
                };
                if p.to_string_lossy().contains("hygiene-reminders") {
                    Ok(serde_json::to_string(&state)?)
                } else {
                    anyhow::bail!("not found")
                }
            }
            fn write(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
            fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> { Ok(()) }
            fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
            fn exists(&self, _: &Path) -> bool { true }
            fn is_dir(&self, p: &Path) -> bool {
                // Parent worktrees dir exists; the orphan child does not.
                !p.to_string_lossy().contains("already-removed")
            }
            fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> {
                anyhow::bail!("not used in this test")
            }
            fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        }

        // Stub git that returns `/repo` as the repo_root for any cwd lookup
        // the hook does. Other methods are unreachable in this code path.
        struct RepoRootGit;
        impl crate::hooks::GitStatusPort for RepoRootGit {
            fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> { Ok(vec![]) }
            fn current_branch(&self, _: &str) -> anyhow::Result<String> { Ok("main".into()) }
            fn is_worktree(&self, _: &str) -> bool { false }
            fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
            fn repo_root(&self, _: &str) -> Option<String> { Some("/repo".into()) }
            fn list_worktree_names(&self, _: &str) -> Vec<String> { Vec::new() }
            fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
            fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
            fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { None }
            fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
            fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
        }

        let fs = DirGoneFs;
        let git = RepoRootGit;
        let stub_proc = crate::hooks::test_support::StubProcess;
        let stub_mcp = crate::hooks::test_support::StubMemoryMcp;
        let stub_env = crate::hooks::test_support::StubEnv::new();
        let ctx = crate::hooks::HookContext {
            git: &git,
            vector_store: None,
            fs: &fs,
            process: &stub_proc,
            llm: None,
            memory_mcp: &stub_mcp,
            env: &stub_env,
        };

        let input = HookInput {
            cwd: Some("/repo".to_string()),
            ..Default::default()
        };

        let output = process_prompt(&input, &ctx);
        // Reminder must NOT be injected when the named worktree dir is gone.
        let injected = output.hook_specific_output
            .as_ref()
            .and_then(|o| o.additional_context.as_deref())
            .unwrap_or("");
        assert!(
            !injected.contains("Worktree Cleanup"),
            "stale dir was removed; reminder should not fire. Got: {injected:?}"
        );
    }
}
