//! `SessionEnd` session-summary hook — write a prose "where we left off" note.
//!
//! Companion to [`super::task_persist`] / [`super::task_rehydrate`]:
//!
//! - `task_persist` (on `TaskCompleted`) writes the **structured** task graph to
//!   `persistent-tasks/{project_hash}/tasks.json` + `meta.json`.
//! - `task_rehydrate` (on `SessionStart`) reads that graph and re-injects it.
//!
//! Neither captures the **narrative**: what shipped (commits landed), what's
//! still in flight, and on which branch. This hook fills that gap. On
//! `SessionEnd` it writes a small `session-summary.json` *beside* `tasks.json`
//! (same `project_hash` dir) holding:
//!   - the recent commit subjects (`git log --oneline -10`) — what shipped,
//!   - the current branch + HEAD sha,
//!   - completed / `in_progress` / pending task counts (read from `tasks.json`),
//!   - session id + timestamp.
//!
//! `task_rehydrate` then reads this on the *next* `SessionStart` and appends a
//! `[Last Session]` prose block right after the structured task list, so the
//! next session resumes with both the task graph AND the human context.
//!
//! **Fail-open everywhere.** `SessionEnd` has a ~1.5s budget (see
//! [`super::session_end`]) so this does exactly one `git log` and one file
//! write — no network, no heavy I/O. Any error (not a repo, no home dir, no
//! tasks) results in `allow()` with nothing written; it never blocks teardown.

use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

/// On-disk shape of the session summary. Kept deliberately small and stable —
/// `task_rehydrate` deserializes the same struct.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session that produced this summary.
    #[serde(default)]
    pub session_id: String,
    /// RFC3339 timestamp the summary was written.
    #[serde(default)]
    pub written_at: String,
    /// Branch HEAD was on at session end.
    #[serde(default)]
    pub branch: String,
    /// Short HEAD sha at session end (empty if unresolved).
    #[serde(default)]
    pub head_sha: String,
    /// Recent commit subjects (newest first), capped to keep the note short.
    #[serde(default)]
    pub recent_commits: Vec<String>,
    /// Count of completed tasks in the persisted task list.
    #[serde(default)]
    pub completed: usize,
    /// Count of `in_progress` tasks.
    #[serde(default)]
    pub in_progress: usize,
    /// Count of pending tasks.
    #[serde(default)]
    pub pending: usize,
}

/// Minimal view of a persisted task — only the fields we count.
#[derive(Debug, serde::Deserialize)]
struct TaskStatusOnly {
    #[serde(default)]
    status: String,
}

/// Compute project hash (must match `task_persist` / `task_rehydrate`).
fn project_hash(cwd: &str) -> String {
    super::project_hash(cwd)
}

/// Directory `persistent-tasks/{project_hash}` (no legacy migration here —
/// `task_rehydrate`/`task_persist` already trigger it on the same run).
fn summary_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(super::persistent_tasks_root(&home).join(project_hash))
}

/// Path to the summary file for a project.
pub fn summary_path(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    Some(summary_dir(fs, project_hash)?.join("session-summary.json"))
}

/// Read the persisted session summary for a project, if one exists and parses.
/// Returns `None` on absence or any parse error (fail-quiet — a corrupt summary
/// must never block `SessionStart`). Used by [`super::task_rehydrate`].
pub fn read_summary(fs: &dyn FileSystemPort, project_hash: &str) -> Option<SessionSummary> {
    let path = summary_path(fs, project_hash)?;
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str::<SessionSummary>(&content).ok()
}

/// Cap on commit subjects we record. 10 is plenty to convey "what shipped"
/// without bloating the `SessionStart` context injection.
const MAX_COMMITS: usize = 10;
/// Hard cap on any single commit-subject length (defensive against a runaway
/// commit message blowing up the note).
const MAX_SUBJECT_LEN: usize = 160;

/// Read recent commit subjects via `git log --oneline`. Returns an empty Vec
/// on any failure (not a repo, git missing, unborn HEAD) — never errors.
fn recent_commits(ctx: &HookContext<'_>, repo: &str) -> Vec<String> {
    let n = MAX_COMMITS.to_string();
    let args = ["log", "--oneline", "--no-color", "-n", n.as_str(), "--format=%s"];
    let Ok(out) = ctx.process.run("git", &args, Some(repo)) else {
        return Vec::new();
    };
    if !out.success {
        return Vec::new();
    }
    out.stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            if l.len() > MAX_SUBJECT_LEN {
                format!("{}…", &l[..MAX_SUBJECT_LEN.min(l.len())])
            } else {
                l.to_string()
            }
        })
        .take(MAX_COMMITS)
        .collect()
}

/// Count tasks by status from the persisted `tasks.json`. Returns
/// `(completed, in_progress, pending)`; all-zero if the file is absent or
/// unparseable (we never want a corrupt task file to block teardown).
fn task_counts(fs: &dyn FileSystemPort, project_hash: &str) -> (usize, usize, usize) {
    let Some(dir) = summary_dir(fs, project_hash) else {
        return (0, 0, 0);
    };
    let path = dir.join("tasks.json");
    let Ok(content) = fs.read_to_string(&path) else {
        return (0, 0, 0);
    };
    let Ok(tasks) = serde_json::from_str::<Vec<TaskStatusOnly>>(&content) else {
        return (0, 0, 0);
    };
    let mut completed = 0;
    let mut in_progress = 0;
    let mut pending = 0;
    for t in &tasks {
        match t.status.as_str() {
            "completed" => completed += 1,
            "in_progress" => in_progress += 1,
            // Treat anything else not-completed (pending, failed, blank) as pending.
            _ => pending += 1,
        }
    }
    (completed, in_progress, pending)
}

/// Build a `SessionSummary` from the current context. Pure-ish: all IO goes
/// through the injected ports so it can be unit-tested with stubs.
fn build_summary(input: &HookInput, ctx: &HookContext<'_>, proj_hash: &str) -> SessionSummary {
    let session_id = input.session_id.as_deref().unwrap_or("unknown").to_string();
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Resolve repo root once; git ops scope to it. Fall back to cwd.
    let repo = ctx.git.repo_root(cwd).unwrap_or_else(|| cwd.to_string());

    let branch = ctx.git.current_branch(&repo).unwrap_or_default();
    let head_sha = ctx
        .git
        .head_sha(&repo)
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_default();
    let recent_commits = recent_commits(ctx, &repo);
    let (completed, in_progress, pending) = task_counts(ctx.fs, proj_hash);

    SessionSummary {
        session_id,
        written_at: chrono::Utc::now().to_rfc3339(),
        branch,
        head_sha,
        recent_commits,
        completed,
        in_progress,
        pending,
    }
}

/// True when the summary carries no signal worth persisting — no commits AND
/// no tasks. Writing an empty note just adds noise to the next `SessionStart`.
fn is_empty_summary(s: &SessionSummary) -> bool {
    s.recent_commits.is_empty() && s.completed == 0 && s.in_progress == 0 && s.pending == 0
}

/// Process `SessionEnd` — write the prose session summary. Always `allow()`.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    let summary = build_summary(input, ctx, &proj_hash);

    if is_empty_summary(&summary) {
        // Nothing to say — don't write an empty note.
        return HookOutput::allow();
    }

    // Resolve path + write. Fail-open on any IO error.
    if let Some(path) = summary_path(ctx.fs, &proj_hash) {
        if let Some(parent) = path.parent() {
            let _ = ctx.fs.create_dir_all(parent);
        }
        match serde_json::to_vec_pretty(&summary) {
            Ok(bytes) => {
                if let Err(e) = ctx.fs.write(&path, &bytes) {
                    tracing::debug!(error = %e, "session_summary: write failed (fail-open)");
                } else {
                    tracing::info!(
                        path = %path.display(),
                        commits = summary.recent_commits.len(),
                        completed = summary.completed,
                        "session_summary: wrote prose summary"
                    );
                }
            }
            Err(e) => tracing::debug!(error = %e, "session_summary: serialize failed (fail-open)"),
        }
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_summary_detected() {
        let s = SessionSummary {
            session_id: "x".into(),
            written_at: "now".into(),
            branch: "main".into(),
            head_sha: "abc".into(),
            recent_commits: vec![],
            completed: 0,
            in_progress: 0,
            pending: 0,
        };
        assert!(is_empty_summary(&s));
    }

    #[test]
    fn non_empty_with_commits() {
        let s = SessionSummary {
            recent_commits: vec!["feat: x".into()],
            ..SessionSummary {
                session_id: String::new(),
                written_at: String::new(),
                branch: String::new(),
                head_sha: String::new(),
                recent_commits: vec![],
                completed: 0,
                in_progress: 0,
                pending: 0,
            }
        };
        assert!(!is_empty_summary(&s));
    }

    #[test]
    fn non_empty_with_tasks() {
        let s = SessionSummary {
            completed: 3,
            ..SessionSummary {
                session_id: String::new(),
                written_at: String::new(),
                branch: String::new(),
                head_sha: String::new(),
                recent_commits: vec![],
                completed: 0,
                in_progress: 0,
                pending: 0,
            }
        };
        assert!(!is_empty_summary(&s));
    }

    #[test]
    fn process_fails_open_without_git_or_tasks() {
        // stub_ctx has no real git/tasks → empty summary → allow(), no panic.
        let input = HookInput {
            session_id: Some("test-session".to_string()),
            cwd: Some("/nonexistent/project".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn summary_roundtrips_through_json() {
        let s = SessionSummary {
            session_id: "sess-1".into(),
            written_at: "2026-05-30T00:00:00Z".into(),
            branch: "feat/x".into(),
            head_sha: "abc123def456".into(),
            recent_commits: vec!["feat: a".into(), "fix: b".into()],
            completed: 2,
            in_progress: 1,
            pending: 4,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn task_counts_zero_when_absent() {
        let ctx = crate::hooks::test_support::stub_ctx();
        // A hash unlikely to have a real tasks.json on disk.
        let (c, i, p) = task_counts(ctx.fs, "zzzzzzzz");
        assert_eq!((c, i, p), (0, 0, 0));
    }

    #[test]
    fn long_commit_subject_is_truncated() {
        // Indirectly: a >MAX_SUBJECT_LEN line gets a trailing ellipsis. We test
        // the truncation arithmetic the mapper uses.
        let long = "x".repeat(MAX_SUBJECT_LEN + 50);
        let truncated = if long.len() > MAX_SUBJECT_LEN {
            format!("{}…", &long[..MAX_SUBJECT_LEN.min(long.len())])
        } else {
            long.clone()
        };
        assert!(truncated.ends_with('…'));
        assert_eq!(truncated.chars().count(), MAX_SUBJECT_LEN + 1);
    }
}
