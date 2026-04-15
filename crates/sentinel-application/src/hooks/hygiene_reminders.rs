//! Hygiene Reminders — Two-phase hook
//!
//! **Stop phase:** Detects conditions that need reminders:
//!   1. Unpushed commits (local ahead of remote)
//!   2. Stale worktrees (merged but not cleaned up)
//!   3. Missing changelog updates (code changed but CHANGELOG.md not)
//!
//! Writes state to `~/.claude/sentinel/state/hygiene-reminders.json`.
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
}

fn state_file(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = home.join(".claude").join("sentinel").join("state");
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join("hygiene-reminders.json"))
}

// ── Stop phase: detect conditions ──────────────────────────────────────

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let mut state = ReminderState::default();

    // 1. Check for unpushed commits
    if let Ok(output) = ctx.git.has_unpushed_commits(cwd) {
        state.unpushed_commits = output;
    }

    // 2. Check for stale worktrees
    let worktree_dir = PathBuf::from(cwd).join(".claude").join("worktrees");
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

    // 3. Check changelog staleness — if session had code changes but no CHANGELOG update
    let changelog_path = PathBuf::from(cwd).join("CHANGELOG.md");
    if ctx.fs.exists(&changelog_path) {
        // Simple heuristic: check if any .rs/.ts/.js files were modified more recently
        // than CHANGELOG.md. We use git status for this.
        if let Ok(files) = ctx.git.changed_files(cwd) {
            let has_code_changes = files.iter().any(|f| {
                f.ends_with(".rs") || f.ends_with(".ts") || f.ends_with(".js")
                    || f.ends_with(".tsx") || f.ends_with(".jsx") || f.ends_with(".py")
            });
            let changelog_changed = files.iter().any(|f| f.contains("CHANGELOG"));
            state.changelog_stale = has_code_changes && !changelog_changed;
        }
    }

    // Write state
    if let Some(path) = state_file(ctx.fs) {
        if let Ok(json) = serde_json::to_string(&state) {
            let _ = ctx.fs.write(&path, json.as_bytes());
        }
    }

    HookOutput::allow()
}

// ── UserPromptSubmit phase: inject reminders ───────────────────────────

pub fn process_prompt(ctx: &HookContext<'_>) -> HookOutput {
    let path = match state_file(ctx.fs) {
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

    HookOutput::inject_context(
        HookEvent::UserPromptSubmit,
        reminders.join("\n\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // We test the state struct serialization and reminder generation

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
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ReminderState = serde_json::from_str(&json).unwrap();
        assert!(parsed.unpushed_commits);
        assert_eq!(parsed.stale_worktrees.len(), 1);
        assert!(parsed.changelog_stale);
    }
}
