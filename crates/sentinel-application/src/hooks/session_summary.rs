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
//! **Fail-visible, non-blocking.** `SessionEnd` has a ~1.5s budget (see
//! [`super::session_end`]) so this does exactly one `git log` and one file
//! write — no network, no heavy I/O. Absence of prior tasks/summary is quiet;
//! corrupt or unwritable Sentinel state injects `[Sentinel-Authority]` context.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::PathBuf;

use super::{concrete_input_session_id, FileSystemPort, HookContext};

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
fn summary_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Result<PathBuf, String> {
    let home = fs
        .home_dir()
        .ok_or_else(|| "cannot determine home directory for session summary".to_string())?;
    Ok(super::persistent_tasks_root(&home).join(project_hash))
}

/// Path to the summary file for a project.
pub fn summary_path(fs: &dyn FileSystemPort, project_hash: &str) -> Result<PathBuf, String> {
    Ok(summary_dir(fs, project_hash)?.join("session-summary.json"))
}

/// Read the persisted session summary for a project, if one exists and parses.
/// Absence is `Ok(None)`; read/parse failures are errors so `SessionStart` can
/// surface corrupt orientation state instead of treating it as empty.
pub fn read_summary(
    fs: &dyn FileSystemPort,
    project_hash: &str,
) -> Result<Option<SessionSummary>, String> {
    let path = summary_path(fs, project_hash)?;
    if !fs.exists(&path) {
        return Ok(None);
    }
    let content = fs
        .read_to_string(&path)
        .map_err(|err| format!("failed to read session summary {}: {err}", path.display()))?;
    serde_json::from_str::<SessionSummary>(&content)
        .map(Some)
        .map_err(|err| format!("failed to parse session summary {}: {err}", path.display()))
}

/// Cap on commit subjects we record. 10 is plenty to convey "what shipped"
/// without bloating the `SessionStart` context injection.
const MAX_COMMITS: usize = 10;
/// Hard cap on any single commit-subject length (defensive against a runaway
/// commit message blowing up the note).
const MAX_SUBJECT_LEN: usize = 160;

/// Read recent commit subjects via `git log --oneline`.
fn recent_commits(ctx: &HookContext<'_>, repo: &str) -> Result<Vec<String>, String> {
    let n = MAX_COMMITS.to_string();
    let args = [
        "log",
        "--oneline",
        "--no-color",
        "-n",
        n.as_str(),
        "--format=%s",
    ];
    let out = ctx
        .process
        .run("git", &args, Some(repo))
        .map_err(|err| format!("failed to read recent commits for session summary: {err}"))?;
    if !out.success {
        return Ok(Vec::new());
    }
    Ok(out
        .stdout
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
        .collect())
}

/// Count tasks by status from the persisted `tasks.json`. Absence means no
/// persisted tasks; read/parse failure is returned so it can be surfaced.
fn task_counts(
    fs: &dyn FileSystemPort,
    project_hash: &str,
) -> Result<Option<(usize, usize, usize)>, String> {
    let dir = summary_dir(fs, project_hash)?;
    let path = dir.join("tasks.json");
    if !fs.exists(&path) {
        return Ok(None);
    }
    let content = fs
        .read_to_string(&path)
        .map_err(|err| format!("failed to read persisted tasks {}: {err}", path.display()))?;
    let tasks = serde_json::from_str::<Vec<TaskStatusOnly>>(&content)
        .map_err(|err| format!("failed to parse persisted tasks {}: {err}", path.display()))?;
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
    Ok(Some((completed, in_progress, pending)))
}

/// Build a `SessionSummary` from the current context. Pure-ish: all IO goes
/// through the injected ports so it can be unit-tested with stubs.
fn build_summary(
    input: &HookInput,
    ctx: &HookContext<'_>,
    proj_hash: &str,
    session_id: &str,
) -> Result<SessionSummary, String> {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let (completed, in_progress, pending) = task_counts(ctx.fs, proj_hash)?.unwrap_or((0, 0, 0));
    let Some(repo) = ctx.git.repo_root(cwd) else {
        if completed == 0 && in_progress == 0 && pending == 0 {
            return Ok(SessionSummary {
                session_id: session_id.to_string(),
                written_at: chrono::Utc::now().to_rfc3339(),
                branch: String::new(),
                head_sha: String::new(),
                recent_commits: Vec::new(),
                completed,
                in_progress,
                pending,
            });
        }
        return Err("failed to resolve repo root for session summary".to_string());
    };

    let recent_commits = recent_commits(ctx, &repo)?;
    if recent_commits.is_empty() && completed == 0 && in_progress == 0 && pending == 0 {
        return Ok(SessionSummary {
            session_id: session_id.to_string(),
            written_at: chrono::Utc::now().to_rfc3339(),
            branch: String::new(),
            head_sha: String::new(),
            recent_commits,
            completed,
            in_progress,
            pending,
        });
    }

    let branch = ctx
        .git
        .current_branch(&repo)
        .map_err(|err| format!("failed to resolve current branch for session summary: {err}"))?;
    let head_sha = ctx
        .git
        .head_sha(&repo)
        .ok_or_else(|| "failed to resolve HEAD sha for session summary".to_string())?
        .chars()
        .take(12)
        .collect::<String>();

    Ok(SessionSummary {
        session_id: session_id.to_string(),
        written_at: chrono::Utc::now().to_rfc3339(),
        branch,
        head_sha,
        recent_commits,
        completed,
        in_progress,
        pending,
    })
}

/// True when the summary carries no signal worth persisting — no commits AND
/// no tasks. Writing an empty note just adds noise to the next `SessionStart`.
fn is_empty_summary(s: &SessionSummary) -> bool {
    s.recent_commits.is_empty() && s.completed == 0 && s.in_progress == 0 && s.pending == 0
}

/// Process `SessionEnd` — write the prose session summary. Always `allow()`.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let Some(session_id) = concrete_input_session_id(input) else {
        tracing::warn!("session_summary skipped durable write without concrete session id");
        return HookOutput::allow();
    };
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    let summary = match build_summary(input, ctx, &proj_hash, session_id) {
        Ok(summary) => summary,
        Err(err) => return authority_context(HookEvent::SessionEnd, err),
    };

    if is_empty_summary(&summary) {
        // Nothing to say — don't write an empty note.
        return HookOutput::allow();
    }

    let path = match summary_path(ctx.fs, &proj_hash) {
        Ok(path) => path,
        Err(err) => return authority_context(HookEvent::SessionEnd, err),
    };
    if let Some(parent) = path.parent() {
        if let Err(err) = ctx.fs.create_dir_all(parent) {
            return authority_context(
                HookEvent::SessionEnd,
                format!(
                    "failed to create session summary dir {}: {err}",
                    parent.display()
                ),
            );
        }
    }
    match serde_json::to_vec_pretty(&summary) {
        Ok(bytes) => {
            if let Err(err) = ctx.fs.write(&path, &bytes) {
                return authority_context(
                    HookEvent::SessionEnd,
                    format!("failed to write session summary {}: {err}", path.display()),
                );
            }
            tracing::info!(
                path = %path.display(),
                commits = summary.recent_commits.len(),
                completed = summary.completed,
                "session_summary: wrote prose summary"
            );
        }
        Err(err) => {
            return authority_context(
                HookEvent::SessionEnd,
                format!("failed to serialize session summary: {err}"),
            );
        }
    }

    HookOutput::allow()
}

pub fn authority_context(event: HookEvent, message: impl Into<String>) -> HookOutput {
    HookOutput::inject_context(
        event,
        format!(
            "{}[Session Summary] {}",
            HookOutput::SENTINEL_AUTHORITY_PREFIX,
            message.into()
        ),
    )
}

pub fn summary_read_error_context(message: impl Into<String>) -> HookOutput {
    authority_context(HookEvent::SessionStart, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};
    use std::path::{Path, PathBuf};

    struct TestFs {
        home: PathBuf,
    }

    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            _: &Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

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
    fn missing_session_does_not_read_or_write_summary_state() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());
        let task_dir = summary_dir(&fs, &project).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("tasks.json"), "not-json").unwrap();

        let input = HookInput {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process(&input, &ctx);
        let context = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());

        assert!(output.blocked.is_none());
        assert!(
            context.is_none(),
            "missing session must not read corrupt task state or inject authority context"
        );
        assert!(!summary_path(&fs, &project).unwrap().exists());
    }

    #[test]
    fn synthetic_session_does_not_write_unknown_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());

        let input = HookInput {
            session_id: Some(" unknown ".to_string()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(!summary_path(&fs, &project).unwrap().exists());
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
        let counts = task_counts(ctx.fs, "zzzzzzzz").unwrap();
        assert_eq!(counts, None);
    }

    #[test]
    fn task_counts_errors_on_corrupt_tasks_json() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestFs {
            home: tmp.path().to_path_buf(),
        };
        let project = "deadbeef";
        let dir = summary_dir(&fs, project).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tasks.json"), "not-json").unwrap();

        let err = task_counts(&fs, project).unwrap_err();
        assert!(err.contains("failed to parse persisted tasks"), "{err}");
    }

    #[test]
    fn read_summary_errors_on_corrupt_summary_json() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestFs {
            home: tmp.path().to_path_buf(),
        };
        let project = "deadbeef";
        let path = summary_path(&fs, project).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not-json").unwrap();

        let err = read_summary(&fs, project).unwrap_err();
        assert!(err.contains("failed to parse session summary"), "{err}");
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
