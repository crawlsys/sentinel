//! Task Coverage Check — Stop hook
//!
//! Keeps the native TaskList current by nudging the agent (reminder only —
//! this hook NEVER writes or auto-changes task status). On each Stop it can
//! emit up to three kinds of reminder:
//!
//! 1. **Uncommitted-but-untracked**: there are uncommitted
//!    file changes but NO `in_progress` task — work is happening off-book.
//!    Debounced: the first off-book Stop warns immediately; while the same
//!    condition persists it re-warns only every
//!    [`OFFBOOK_REMINDER_INTERVAL`] stops; when the condition clears the
//!    counter resets so the next off-book episode warns immediately again.
//!
//! 2. **Done-signal nudge**: there is ≥1 `in_progress` task AND this turn
//!    produced a "looks done" signal — a new commit since the last Stop (HEAD
//!    SHA changed), a PR reference, or a successful test/build run in the last
//!    assistant message. Prompts the agent to mark the task ✅ via `TaskUpdate`.
//!    (Event-driven, so it does not loop and needs no debounce.)
//!
//! 3. **Stale nudge**: an `in_progress` task has persisted across ≥3
//!    consecutive Stop events without leaving the in-progress set. Prompts the
//!    agent to update status or confirm the task is still active. Debounced:
//!    fires at exact multiples of the threshold (stops 3, 6, 9, …), not on
//!    every stop past it.
//!
//! **Fail-visible contract**: task/git/state read failures inject explicit
//! `[Sentinel-Authority]` context. This hook still never blocks Stop, but it
//! does not accept unknown coverage state.
//!
//! **Reminder only**: all signals route through [`HookOutput::inject_context`]
//! so the *agent* decides whether to call `TaskUpdate`. Sentinel does not write
//! task status itself.
//!
//! ## State markers (per session, under `~/.claude/sentinel/state/`)
//! - `coverage-headsha-{session_id}` — last-seen `HEAD` SHA. A change between
//!   consecutive Stops is the commit "done signal".
//! - `coverage-inprogress-{session_id}` — `id=stop-count` lines tracking how
//!   many consecutive Stops each in-progress task has persisted (drives the
//!   stale nudge).
//! - `coverage-offbook-{session_id}` — count of consecutive Stops the
//!   off-book condition (uncommitted changes, no in-progress task) has
//!   persisted (drives the debounced uncommitted-but-untracked warning).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{concrete_input_session_id, FileSystemPort, HookContext};

/// Number of consecutive Stop events an `in_progress` task may persist
/// (unchanged) before the stale nudge fires. The nudge repeats at exact
/// multiples of this threshold (3, 6, 9, …) rather than on every stop past it.
const STALE_STOP_THRESHOLD: u32 = 3;

/// While the off-book condition persists, the uncommitted-but-untracked warning
/// re-fires every configured interval after warning immediately on the first
/// Stop. Keeps the reminder alive without repeating it verbatim on every turn.
const OFFBOOK_REMINDER_INTERVAL: u32 = 5;

/// Minimal task shape — only the fields this hook needs. Matches Claude Code's
/// on-disk task JSON; extra fields are ignored.
#[derive(Debug, Clone, serde::Deserialize)]
struct Task {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    status: String,
}

fn authority_context(message: impl Into<String>) -> HookOutput {
    HookOutput::inject_context(
        HookEvent::Stop,
        format!(
            "{}[Task Coverage] {}",
            HookOutput::SENTINEL_AUTHORITY_PREFIX,
            message.into()
        ),
    )
}

/// Read all tasks from the active session task dir
/// (`~/.claude/tasks/{session_id}/`).
///
/// Strictly session-scoped — mirrors `task_persist::find_active_task_dir` so
/// the two hooks read the exact same set of task files. An absent task dir is a
/// valid empty set; unreadable or malformed task files are reported.
fn read_active_tasks(fs: &dyn FileSystemPort, session_id: &str) -> Result<Vec<Task>, String> {
    let home = fs
        .home_dir()
        .ok_or_else(|| "cannot determine home directory for task lookup".to_string())?;
    let session_dir = super::session_task_dir(fs, &home, session_id);
    if !fs.is_dir(&session_dir) {
        return Ok(Vec::new());
    }

    let entries = fs
        .read_dir(&session_dir)
        .map_err(|err| format!("failed to list task dir {}: {err}", session_dir.display()))?;
    let mut tasks = Vec::new();
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !Path::new(&name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            || name.starts_with('.')
        {
            continue;
        }

        let content = fs
            .read_to_string(&path)
            .map_err(|err| format!("failed to read task file {}: {err}", path.display()))?;
        let task = serde_json::from_str::<Task>(&content)
            .map_err(|err| format!("failed to parse task file {}: {err}", path.display()))?;
        tasks.push(task);
    }
    Ok(tasks)
}

/// Per-session state dir: `~/.claude/sentinel/state`.
fn state_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(home.join(".claude").join("sentinel").join("state"))
}

/// Path for the last-seen HEAD SHA marker for this session.
fn head_sha_marker(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    Some(state_dir(fs)?.join(format!("coverage-headsha-{session_id}")))
}

/// Path for the in-progress stop-count marker for this session.
fn inprogress_marker(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    Some(state_dir(fs)?.join(format!("coverage-inprogress-{session_id}")))
}

/// Path for the consecutive off-book stop-count marker for this session.
fn offbook_marker(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    Some(state_dir(fs)?.join(format!("coverage-offbook-{session_id}")))
}

/// Read the consecutive off-book stop count. An absent or empty file is a
/// valid zero; unreadable or malformed content is reported.
fn read_offbook_count(fs: &dyn FileSystemPort, session_id: &str) -> Result<u32, String> {
    let path = offbook_marker(fs, session_id)
        .ok_or_else(|| "cannot determine state dir for coverage off-book marker".to_string())?;
    if !fs.exists(&path) {
        return Ok(0);
    }
    let content = fs.read_to_string(&path).map_err(|err| {
        format!(
            "failed to read coverage off-book marker {}: {err}",
            path.display()
        )
    })?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    trimmed.parse::<u32>().map_err(|err| {
        format!(
            "invalid stop count in coverage off-book marker {}: {err}",
            path.display()
        )
    })
}

/// Persist the consecutive off-book stop count.
fn write_offbook_count(
    fs: &dyn FileSystemPort,
    session_id: &str,
    count: u32,
) -> Result<(), String> {
    let path = offbook_marker(fs, session_id)
        .ok_or_else(|| "cannot determine state dir for coverage off-book marker".to_string())?;
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create coverage state dir {}: {err}",
                parent.display()
            )
        })?;
    }
    fs.write(&path, count.to_string().as_bytes())
        .map_err(|err| {
            format!(
                "failed to write coverage off-book marker {}: {err}",
                path.display()
            )
        })
}

/// Read the previously-recorded HEAD SHA for this session (trimmed). Empty or
/// absent file means "no prior SHA seen"; read failures are reported.
fn read_prev_head_sha(fs: &dyn FileSystemPort, session_id: &str) -> Result<Option<String>, String> {
    let path = head_sha_marker(fs, session_id)
        .ok_or_else(|| "cannot determine state dir for coverage HEAD marker".to_string())?;
    if !fs.exists(&path) {
        return Ok(None);
    }
    let content = fs.read_to_string(&path).map_err(|err| {
        format!(
            "failed to read coverage HEAD marker {}: {err}",
            path.display()
        )
    })?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Persist the current HEAD SHA for this session.
fn write_head_sha(fs: &dyn FileSystemPort, session_id: &str, sha: &str) -> Result<(), String> {
    let path = head_sha_marker(fs, session_id)
        .ok_or_else(|| "cannot determine state dir for coverage HEAD marker".to_string())?;
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create coverage state dir {}: {err}",
                parent.display()
            )
        })?;
    }
    fs.write(&path, sha.as_bytes()).map_err(|err| {
        format!(
            "failed to write coverage HEAD marker {}: {err}",
            path.display()
        )
    })
}

/// Read the `id -> consecutive-stop-count` map for in-progress tasks. Stored as
/// `id=count` lines so it stays trivially parseable. Malformed entries are
/// reported instead of ignored.
fn read_inprogress_counts(
    fs: &dyn FileSystemPort,
    session_id: &str,
) -> Result<BTreeMap<String, u32>, String> {
    let mut map = BTreeMap::new();
    let path = inprogress_marker(fs, session_id)
        .ok_or_else(|| "cannot determine state dir for coverage in-progress marker".to_string())?;
    if !fs.exists(&path) {
        return Ok(map);
    }
    let content = fs.read_to_string(&path).map_err(|err| {
        format!(
            "failed to read coverage in-progress marker {}: {err}",
            path.display()
        )
    })?;
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (id, count) = line.split_once('=').ok_or_else(|| {
            format!(
                "malformed coverage in-progress marker {} line {}",
                path.display(),
                idx + 1
            )
        })?;
        let n = count.trim().parse::<u32>().map_err(|err| {
            format!(
                "invalid stop count in coverage marker {} line {}: {err}",
                path.display(),
                idx + 1
            )
        })?;
        map.insert(id.trim().to_string(), n);
    }
    Ok(map)
}

/// Persist the in-progress stop-count map.
fn write_inprogress_counts(
    fs: &dyn FileSystemPort,
    session_id: &str,
    map: &BTreeMap<String, u32>,
) -> Result<(), String> {
    let path = inprogress_marker(fs, session_id)
        .ok_or_else(|| "cannot determine state dir for coverage in-progress marker".to_string())?;
    if let Some(parent) = path.parent() {
        fs.create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create coverage state dir {}: {err}",
                parent.display()
            )
        })?;
    }
    let mut body = String::new();
    for (id, count) in map {
        body.push_str(id);
        body.push('=');
        body.push_str(&count.to_string());
        body.push('\n');
    }
    fs.write(&path, body.as_bytes()).map_err(|err| {
        format!(
            "failed to write coverage in-progress marker {}: {err}",
            path.display()
        )
    })
}

/// Detect a non-commit "done" signal in the last assistant message: a PR
/// reference (open/merged) or a successful test/build run. Commit detection is
/// handled separately via the HEAD-SHA marker.
fn message_has_done_signal(msg: &str) -> bool {
    let lower = msg.to_lowercase();

    // PR references — created, opened, or merged.
    let pr_signal = (lower.contains("github.com/") && lower.contains("/pull/"))
        || lower.contains("gh pr create")
        || lower.contains("pull request")
        || lower.contains("opened pr")
        || lower.contains("merged pr")
        || lower.contains("pr merged")
        || lower.contains("pr #")
        || lower.contains("merged to main")
        || lower.contains("merged into main");

    // Successful test / build run.
    let test_build_signal = lower.contains("test result: ok")
        || lower.contains("tests passed")
        || lower.contains("all tests pass")
        || lower.contains("test passed")
        || lower.contains("0 failed")
        || lower.contains("build succeeded")
        || lower.contains("build successful")
        || lower.contains("finished `release`")
        || lower.contains("finished `dev`");

    pr_signal || test_build_signal
}

/// Build the human-readable "id (subject)" list for a set of tasks, capped so
/// the injected reminder stays compact.
fn format_task_refs(tasks: &[&Task]) -> String {
    tasks
        .iter()
        .take(8)
        .map(|t| {
            if t.subject.is_empty() {
                format!("#{}", t.id)
            } else {
                format!("#{} ({})", t.id, t.subject)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let Some(session_id) = concrete_input_session_id(input) else {
        return authority_context(
            "Stop event did not include a concrete session_id; task coverage state cannot be verified.",
        );
    };

    // ---- Read the task list (same source as task_persist) ----------------
    let tasks = match read_active_tasks(ctx.fs, session_id) {
        Ok(tasks) => tasks,
        Err(err) => return authority_context(err),
    };

    let in_progress: Vec<&Task> = tasks.iter().filter(|t| t.status == "in_progress").collect();

    // ---- Commit signal: did HEAD move since the last Stop? ---------------
    // Read the prior SHA *before* overwriting it; the marker is the source of
    // truth for "new commit this turn".
    let prev_sha = match read_prev_head_sha(ctx.fs, session_id) {
        Ok(prev) => prev,
        Err(err) => return authority_context(err),
    };
    let cur_sha = ctx.git.head_sha(cwd);
    let head_changed = match (&prev_sha, &cur_sha) {
        (Some(prev), Some(cur)) => prev != cur,
        // First observation (no prior marker) is NOT a commit signal — we only
        // know HEAD moved if we've previously recorded a different value.
        _ => false,
    };
    // Always refresh the marker so the next Stop compares against this turn.
    if let Some(cur) = &cur_sha {
        if let Err(err) = write_head_sha(ctx.fs, session_id, cur) {
            return authority_context(err);
        }
    }

    // ---- Stale tracking: bump/reset per-task stop counts -----------------
    // The map only ever holds currently-in_progress ids. Tasks that leave
    // in_progress are dropped (their count resets implicitly on re-entry).
    let prev_counts = match read_inprogress_counts(ctx.fs, session_id) {
        Ok(counts) => counts,
        Err(err) => return authority_context(err),
    };
    let mut new_counts: BTreeMap<String, u32> = BTreeMap::new();
    for t in &in_progress {
        if t.id.is_empty() {
            continue;
        }
        let next = prev_counts.get(&t.id).copied().unwrap_or(0) + 1;
        new_counts.insert(t.id.clone(), next);
    }
    if let Err(err) = write_inprogress_counts(ctx.fs, session_id, &new_counts) {
        return authority_context(err);
    }

    // ---- Decide which reminder (if any) to inject ------------------------

    // No in_progress task: uncommitted work is happening off-book. Debounced:
    // warn on the first off-book Stop, then only every
    // OFFBOOK_REMINDER_INTERVAL stops while the condition persists; reset the
    // episode counter as soon as the condition clears.
    if in_progress.is_empty() {
        let has_changes = match ctx.git.has_uncommitted_changes(cwd) {
            Ok(has_changes) => has_changes,
            Err(err) => {
                // The Stop's cwd is not inside a git repository (the common
                // case: the event arrived without a `cwd` field, so the `.`
                // fallback resolved to the evaluating process's working
                // directory — e.g. "/" under the systemd daemon). Skip
                // silently using the same probe `git_hygiene` uses, instead
                // of surfacing an [Sentinel-Authority] error. That error path
                // bypasses the off-book debounce and would otherwise fire on
                // every single Stop.
                if ctx.git.repo_root(cwd).is_none() {
                    return HookOutput::allow();
                }
                // Genuine git failure inside a real repo: surface it the same
                // way off-book work is surfaced, but gated by the debounce
                // counter so a persistent failure can never loop un-debounced.
                let prev_offbook = match read_offbook_count(ctx.fs, session_id) {
                    Ok(count) => count,
                    Err(_) => return HookOutput::allow(),
                };
                let count = prev_offbook.saturating_add(1);
                let _ = write_offbook_count(ctx.fs, session_id, count);
                if count != 1 && count % OFFBOOK_REMINDER_INTERVAL != 0 {
                    return HookOutput::allow();
                }
                return authority_context(format!(
                    "failed to inspect git worktree changes for task coverage: {err}"
                ));
            }
        };
        let prev_offbook = match read_offbook_count(ctx.fs, session_id) {
            Ok(count) => count,
            Err(err) => return authority_context(err),
        };
        if !has_changes {
            if prev_offbook != 0 {
                if let Err(err) = write_offbook_count(ctx.fs, session_id, 0) {
                    return authority_context(err);
                }
            }
            return HookOutput::allow();
        }
        let count = prev_offbook.saturating_add(1);
        if let Err(err) = write_offbook_count(ctx.fs, session_id, count) {
            return authority_context(err);
        }
        if count != 1 && count % OFFBOOK_REMINDER_INTERVAL != 0 {
            return HookOutput::allow();
        }
        let context = "[Task Coverage] WARNING: Uncommitted file changes detected but no task is \
             in_progress. All work should be tracked as a task. Create a task with `TaskCreate` \
             and mark it `in_progress` with `TaskUpdate` to track this work.";
        return HookOutput::inject_context(HookEvent::Stop, context);
    }

    // An in_progress task exists, so any off-book episode is over — reset the
    // counter so the next episode warns immediately.
    let prev_offbook = match read_offbook_count(ctx.fs, session_id) {
        Ok(count) => count,
        Err(err) => return authority_context(err),
    };
    if prev_offbook != 0 {
        if let Err(err) = write_offbook_count(ctx.fs, session_id, 0) {
            return authority_context(err);
        }
    }

    // There IS at least one in_progress task. Done-signal first (commit, PR, or
    // successful test/build), then staleness.
    let last_msg = input.last_assistant_message.as_deref().unwrap_or("");
    let done_signal = head_changed || message_has_done_signal(last_msg);

    if done_signal {
        let refs = format_task_refs(&in_progress);
        let what = if head_changed {
            "committed"
        } else {
            "committed/PR'd or ran a passing test/build"
        };
        let context = format!(
            "[Task Coverage] Task(s) {refs} are in_progress and you just {what} — \
             mark them ✅ with `TaskUpdate` if complete (or leave them in_progress if there's \
             more to do). Reminder only; sentinel does not change task status for you."
        );
        return HookOutput::inject_context(HookEvent::Stop, context);
    }

    // Stale check: any in_progress task that has persisted ≥ threshold stops.
    // Debounced to exact multiples of the threshold (3, 6, 9, …) so the nudge
    // repeats periodically instead of on every stop past the threshold.
    let stale: Vec<&Task> = in_progress
        .iter()
        .filter(|t| {
            let count = new_counts.get(&t.id).copied().unwrap_or(0);
            count >= STALE_STOP_THRESHOLD && count % STALE_STOP_THRESHOLD == 0
        })
        .copied()
        .collect();
    if !stale.is_empty() {
        let refs = format_task_refs(&stale);
        let context = format!(
            "[Task Coverage] Task(s) {refs} have been in_progress for a while — \
             update status (🔄→✅/❌) with `TaskUpdate` or confirm they're still active. \
             Reminder only; sentinel does not change task status for you."
        );
        return HookOutput::inject_context(HookEvent::Stop, context);
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{StubEnv, StubMemoryMcp, StubProcess};
    use sentinel_domain::ports::GitStatusPort;
    use std::cell::RefCell;
    use std::path::Path;

    /// Real-FS adapter scoped to a temp home so state markers and task files
    /// stay isolated per test.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for ScopedHomeFs {
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
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
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
                .filter_map(|e| e.ok().map(|e| e.path()))
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

    /// FS whose every read fails — used to exercise visible authority errors.
    struct UnreadableFs {
        home: PathBuf,
    }
    impl FileSystemPort for UnreadableFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            _: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "boom",
            ))
        }
        fn write(
            &self,
            _: &Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "boom",
            ))
        }
        fn create_dir_all(
            &self,
            _: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "boom",
            ))
        }
        fn read_dir(
            &self,
            _: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "boom",
            ))
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            // Claim the tasks dir exists so read_dir gets exercised and fails.
            true
        }
        fn metadata(
            &self,
            _: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "boom",
            ))
        }
        fn append(
            &self,
            _: &Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::backend(
                "boom",
            ))
        }
    }

    /// Git stub returning a caller-chosen HEAD SHA + uncommitted-changes flag.
    struct FakeGit {
        head: RefCell<Option<String>>,
        uncommitted: bool,
    }
    impl GitStatusPort for FakeGit {
        fn has_uncommitted_changes(
            &self,
            _: &str,
        ) -> Result<bool, sentinel_domain::port_errors::GitError> {
            Ok(self.uncommitted)
        }
        fn changed_files(
            &self,
            _: &str,
        ) -> Result<Vec<String>, sentinel_domain::port_errors::GitError> {
            Ok(vec![])
        }
        fn current_branch(
            &self,
            _: &str,
        ) -> Result<String, sentinel_domain::port_errors::GitError> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(
            &self,
            _: &str,
        ) -> Result<bool, sentinel_domain::port_errors::GitError> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn rev_list_count_range(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn head_sha(&self, _: &str) -> Option<String> {
            self.head.borrow().clone()
        }
    }

    /// Build a HookContext over the given fs + git.
    fn ctx_with<'a>(
        fs: &'a dyn FileSystemPort,
        git: &'a dyn GitStatusPort,
        process: &'a StubProcess,
        memory_mcp: &'a StubMemoryMcp,
        env: &'a StubEnv,
    ) -> HookContext<'a> {
        HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        }
    }

    /// Seed a session task dir with the given (id, status) tasks.
    fn seed_tasks(home: &Path, session_id: &str, tasks: &[(&str, &str)]) {
        let dir = home.join(".claude").join("tasks").join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        for (id, status) in tasks {
            std::fs::write(
                dir.join(format!("{id}.json")),
                format!(r#"{{"id":"{id}","subject":"Task {id}","status":"{status}"}}"#),
            )
            .unwrap();
        }
    }

    fn input_for(session_id: &str, cwd: &str, last_msg: Option<&str>) -> HookInput {
        HookInput {
            session_id: Some(session_id.to_string()),
            cwd: Some(cwd.to_string()),
            last_assistant_message: last_msg.map(str::to_string),
            ..Default::default()
        }
    }

    /// Extract injected additional-context text from a HookOutput, if any.
    fn injected_text(out: &HookOutput) -> Option<String> {
        out.hook_specific_output
            .as_ref()
            .and_then(|hso| hso.additional_context.clone())
    }

    #[test]
    fn done_signal_injects_when_in_progress_and_head_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-commit";
        seed_tasks(&home, sid, &[("1", "in_progress"), ("2", "pending")]);

        let fs = ScopedHomeFs { home: home.clone() };
        // Pre-seed the prior HEAD marker with an OLD sha; current differs.
        write_head_sha(&fs, sid, "oldsha111").unwrap();
        let git = FakeGit {
            head: RefCell::new(Some("newsha222".to_string())),
            uncommitted: true,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(sid, home.to_str().unwrap(), None);
        let out = process(&input, &ctx);
        let msg = injected_text(&out).expect("expected a done-signal injection");
        assert!(msg.contains("in_progress"));
        assert!(msg.contains("just committed"));
        assert!(msg.contains("#1"));
        assert!(!msg.contains("#2"), "only in_progress tasks listed");
    }

    #[test]
    fn done_signal_injects_on_pr_reference_in_message() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-pr";
        seed_tasks(&home, sid, &[("5", "in_progress")]);

        let fs = ScopedHomeFs { home: home.clone() };
        // Same HEAD both times → no commit signal; rely on PR text.
        write_head_sha(&fs, sid, "samesha").unwrap();
        let git = FakeGit {
            head: RefCell::new(Some("samesha".to_string())),
            uncommitted: false,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(
            sid,
            home.to_str().unwrap(),
            Some("Opened PR https://github.com/acme/repo/pull/42 for review."),
        );
        let out = process(&input, &ctx);
        let msg = injected_text(&out).expect("expected a done-signal injection from PR ref");
        assert!(msg.contains("#5"));
    }

    #[test]
    fn stale_injects_after_three_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-stale";
        seed_tasks(&home, sid, &[("9", "in_progress")]);

        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();

        // No HEAD movement, no done text → only staleness can fire. Debounced:
        // nudges land on exact multiples of the threshold (stops 3 and 6) and
        // are suppressed in between (stops 4 and 5).
        for stop in 1..=6 {
            let fs = ScopedHomeFs { home: home.clone() };
            let git = FakeGit {
                head: RefCell::new(Some("stable".to_string())),
                uncommitted: false,
            };
            let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);
            let input = input_for(sid, home.to_str().unwrap(), Some("still working"));
            let out = process(&input, &ctx);
            if stop % 3 == 0 {
                let msg = injected_text(&out).expect("stops 3 and 6 must trigger the stale nudge");
                assert!(msg.contains("in_progress for a while"));
                assert!(msg.contains("#9"));
            } else {
                assert!(
                    injected_text(&out).is_none(),
                    "stop {stop} must NOT nudge (below threshold or debounced)"
                );
            }
        }
    }

    #[test]
    fn off_book_warning_debounced_after_first_fire() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-offbook-debounce";
        seed_tasks(&home, sid, &[("1", "pending")]);

        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();

        // Condition persists across 5 stops → warn at 1, silent 2–4, re-warn
        // at OFFBOOK_REMINDER_INTERVAL.
        for stop in 1..=5 {
            let fs = ScopedHomeFs { home: home.clone() };
            let git = FakeGit {
                head: RefCell::new(Some("x".to_string())),
                uncommitted: true,
            };
            let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);
            let input = input_for(sid, home.to_str().unwrap(), None);
            let out = process(&input, &ctx);
            if stop == 1 || stop == 5 {
                let msg = injected_text(&out)
                    .unwrap_or_else(|| panic!("stop {stop} must warn (first fire / interval)"));
                assert!(msg.contains("Uncommitted file changes detected"));
            } else {
                assert!(
                    injected_text(&out).is_none(),
                    "stop {stop} must be debounced"
                );
            }
        }
    }

    #[test]
    fn off_book_counter_resets_when_condition_clears() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-offbook-reset";
        seed_tasks(&home, sid, &[("1", "pending")]);

        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();

        let run = |uncommitted: bool| {
            let fs = ScopedHomeFs { home: home.clone() };
            let git = FakeGit {
                head: RefCell::new(Some("x".to_string())),
                uncommitted,
            };
            let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);
            let input = input_for(sid, home.to_str().unwrap(), None);
            process(&input, &ctx)
        };

        // Episode 1: first off-book stop warns.
        let out = run(true);
        assert!(injected_text(&out).is_some(), "episode 1 must warn");
        // Condition clears (clean tree) → counter resets, no warning.
        let out = run(false);
        assert!(injected_text(&out).is_none(), "clean tree must not warn");
        // Episode 2: condition returns → warns immediately, not debounced.
        let out = run(true);
        assert!(
            injected_text(&out).is_some(),
            "a new off-book episode must warn immediately after a reset"
        );
    }

    #[test]
    fn no_in_progress_no_nudge_when_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-clean";
        seed_tasks(&home, sid, &[("1", "pending"), ("2", "completed")]);

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit {
            head: RefCell::new(Some("x".to_string())),
            uncommitted: false, // clean tree → no untracked-work warning either
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(
            sid,
            home.to_str().unwrap(),
            Some("done with a commit pushed"),
        );
        let out = process(&input, &ctx);
        assert!(
            injected_text(&out).is_none(),
            "no in_progress task + clean tree → no nudge"
        );
    }

    #[test]
    fn no_in_progress_but_uncommitted_injects_off_book_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-off-book";
        seed_tasks(&home, sid, &[("1", "pending")]);

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit {
            head: RefCell::new(Some("x".to_string())),
            uncommitted: true,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(sid, home.to_str().unwrap(), None);
        let out = process(&input, &ctx);
        let msg = injected_text(&out).expect("off-book uncommitted warning must fire");
        assert!(msg.contains("no task is"));
        assert!(msg.contains("in_progress"));
    }

    /// Git stub simulating a Stop whose `cwd` is NOT inside a git repository
    /// (the live failure mode: a Stop arrived without `cwd`, so the `."`
    /// fallback resolved to the daemon's working directory). Mirrors `FakeGit`
    /// but with `has_uncommitted_changes` erroring and `repo_root` returning
    /// `None`, exactly as the real `RealGit` does on a non-repo path.
    struct NonRepoGit {
        head: RefCell<Option<String>>,
    }
    impl GitStatusPort for NonRepoGit {
        fn has_uncommitted_changes(
            &self,
            _: &str,
        ) -> Result<bool, sentinel_domain::port_errors::GitError> {
            Err(sentinel_domain::port_errors::GitError::backend(
                "Failed to run git status: fatal: not a git repository",
            ))
        }
        fn changed_files(
            &self,
            _: &str,
        ) -> Result<Vec<String>, sentinel_domain::port_errors::GitError> {
            Ok(vec![])
        }
        fn current_branch(
            &self,
            _: &str,
        ) -> Result<String, sentinel_domain::port_errors::GitError> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(
            &self,
            _: &str,
        ) -> Result<bool, sentinel_domain::port_errors::GitError> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn rev_list_count_range(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn head_sha(&self, _: &str) -> Option<String> {
            self.head.borrow().clone()
        }
    }

    /// Regression: when `has_uncommitted_changes` fails because the Stop's cwd
    /// is not a git repository, the hook must skip silently — NOT inject a
    /// `[Sentinel-Authority]` error. The buggy code returned `authority_context`
    /// on every Stop (the error path bypassed the off-book debounce), producing
    /// an un-debounced loop. Non-repo cwd is the live trigger.
    #[test]
    fn non_repo_cwd_skips_silently_instead_of_error_loop() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-nonrepo";
        // No in_progress task, so the no-coverage branch is the one under test.
        seed_tasks(&home, sid, &[("1", "pending")]);

        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();

        // Every Stop in this scenario looks identical (non-repo cwd). If the
        // fix is correct, NONE of them inject; if the bug is present, ALL do.
        for stop in 1..=6 {
            let fs = ScopedHomeFs { home: home.clone() };
            let git = NonRepoGit {
                head: RefCell::new(None),
            };
            let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);
            // cwd points outside any repo, mirroring the daemon's "/" case.
            let input = input_for(sid, "/", None);
            let out = process(&input, &ctx);
            assert!(
                injected_text(&out).is_none(),
                "non-repo cwd on stop {stop} must skip silently, not inject a \
                 [Sentinel-Authority] error (un-debounced loop)"
            );
            assert!(out.blocked.is_none());
        }
    }

    #[test]
    fn temp_active_marker_does_not_suppress_off_book_warning() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-no-temp-bypass";
        seed_tasks(&home, sid, &[("1", "pending")]);

        let active_marker = std::env::temp_dir().join(format!("claude-task-active-{sid}"));
        std::fs::write(&active_marker, b"old marker").unwrap();

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit {
            head: RefCell::new(Some("x".to_string())),
            uncommitted: true,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(sid, home.to_str().unwrap(), None);
        let out = process(&input, &ctx);
        let _ = std::fs::remove_file(&active_marker);

        let msg = injected_text(&out).expect("off-book warning must ignore temp marker");
        assert!(msg.contains("Uncommitted file changes detected"));
        assert!(msg.contains("in_progress"));
    }

    #[test]
    fn unreadable_state_injects_authority_context() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-broken";

        let fs = UnreadableFs { home };
        let git = FakeGit {
            head: RefCell::new(Some("x".to_string())),
            uncommitted: true,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(sid, "/tmp/whatever", Some("anything"));
        let out = process(&input, &ctx);
        let msg = injected_text(&out).expect("unreadable state must be visible");
        assert!(msg.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(msg.contains("failed to list task dir"));
        assert!(out.blocked.is_none());
    }

    #[test]
    fn no_session_id_injects_authority_context() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput {
            session_id: None,
            cwd: Some("/tmp".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        let msg = injected_text(&out).expect("missing session id must be visible");
        assert!(msg.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(msg.contains("session_id"));
        assert!(out.blocked.is_none());
    }

    #[test]
    fn synthetic_unknown_session_injects_authority_context_without_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit {
            head: RefCell::new(Some("headsha".to_string())),
            uncommitted: false,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = HookInput {
            session_id: Some(" unknown ".to_string()),
            cwd: Some(home.to_string_lossy().to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        let msg = injected_text(&out).expect("synthetic session id must be visible");
        assert!(msg.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(msg.contains("concrete session_id"));
        assert!(!home
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("coverage-headsha- unknown ")
            .exists());
        assert!(!home
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("coverage-headsha-unknown")
            .exists());
        assert!(out.blocked.is_none());
    }

    #[test]
    fn first_observation_is_not_a_commit_signal() {
        // No prior HEAD marker → head_changed must be false even though a SHA
        // exists, so an in_progress task with no other signal stays quiet.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-first";
        seed_tasks(&home, sid, &[("3", "in_progress")]);

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit {
            head: RefCell::new(Some("firstsha".to_string())),
            uncommitted: false,
        };
        let proc_stub = StubProcess;
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc_stub, &mem, &env);

        let input = input_for(sid, home.to_str().unwrap(), Some("working on it"));
        let out = process(&input, &ctx);
        assert!(
            injected_text(&out).is_none(),
            "first Stop with no prior SHA must not fire a done-signal"
        );
    }

    #[test]
    fn message_done_signal_matches_test_ok() {
        assert!(message_has_done_signal(
            "test result: ok. 12 passed; 0 failed"
        ));
        assert!(message_has_done_signal("Build succeeded with no errors"));
        assert!(message_has_done_signal("Opened pull request #7"));
        assert!(!message_has_done_signal("still investigating the bug"));
    }
}
