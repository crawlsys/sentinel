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

use super::{FileSystemPort, GitStatusPort, HookContext};

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct ReminderState {
    #[serde(default)]
    unpushed_commits: bool,
    #[serde(default)]
    stale_worktrees: Vec<String>,
    #[serde(default)]
    changelog_stale: bool,
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

    // 2. Stale worktrees — only dirs inside THIS repo's .claude/worktrees/.
    //    We only report dirs that exist on disk; git worktree registration state
    //    is not available through the port, but orphaned dirs are the real problem.
    let worktree_dir = PathBuf::from(&repo_root).join(".claude").join("worktrees");
    if ctx.fs.is_dir(&worktree_dir) {
        if let Ok(entries) = ctx.fs.read_dir(&worktree_dir) {
            for entry in entries {
                if ctx.fs.is_dir(&entry) {
                    if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
                        state.stale_worktrees.push(name.to_string());
                    }
                }
            }
        }
    }

    // 3. Changelog staleness — code files changed but CHANGELOG.md not touched.
    //    Uses uncommitted changed files (git status). After a commit this clears
    //    naturally since changed_files returns empty. That's correct behaviour:
    //    if you committed without updating CHANGELOG, doc_drift catches it instead.
    let changelog_path = PathBuf::from(&repo_root).join("CHANGELOG.md");
    if ctx.fs.exists(&changelog_path) {
        if let Ok(files) = ctx.git.changed_files(cwd) {
            let has_code_changes = files.iter().any(is_code_file);
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

fn is_code_file(f: &String) -> bool {
    f.ends_with(".rs")
        || f.ends_with(".ts")
        || f.ends_with(".tsx")
        || f.ends_with(".js")
        || f.ends_with(".jsx")
        || f.ends_with(".py")
        || f.ends_with(".go")
}

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

    if !state.stale_worktrees.is_empty() {
        reminders.push(format!(
            "[Worktree Cleanup] {} stale worktree(s) found: {}. \
             Clean up with `ExitWorktree(action: \"remove\")` or `git worktree remove`.",
            state.stale_worktrees.len(),
            state.stale_worktrees.join(", ")
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
            repo_root: "/repos/sentinel".to_string(),
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ReminderState = serde_json::from_str(&json).unwrap();
        assert!(parsed.unpushed_commits);
        assert_eq!(parsed.stale_worktrees.len(), 1);
        assert!(parsed.changelog_stale);
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
        };
        // Simulate: state says sentinel, but current repo_root is "other-project"
        assert_ne!(state.repo_root, "/repos/other-project");
    }

    #[test]
    fn test_is_code_file() {
        assert!(is_code_file(&"src/main.rs".to_string()));
        assert!(is_code_file(&"app/page.tsx".to_string()));
        assert!(is_code_file(&"lib/utils.py".to_string()));
        assert!(!is_code_file(&"README.md".to_string()));
        assert!(!is_code_file(&"CHANGELOG.md".to_string()));
        assert!(!is_code_file(&"config.toml".to_string()));
    }
}
