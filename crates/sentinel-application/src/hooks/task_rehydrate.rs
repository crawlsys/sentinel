//! Task Rehydrate Hook — inject persistent tasks on `SessionStart`
//!
//! Fires on `SessionStart`. Reads `~/.claude/sentinel/persistent-tasks/{project_hash}/tasks.json`
//! and injects incomplete tasks into context as a system reminder so Claude
//! sees prior work and can continue where the previous session left off.
//!
//! Only injects tasks that are NOT completed — completed tasks are mentioned
//! as a summary count but not listed in full.

use chrono::Utc;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::fmt::Write as _;
use std::path::PathBuf;

use super::{concrete_input_session_id, FileSystemPort, HookContext};

/// A single checklist item within a task
#[derive(Debug, Clone, serde::Deserialize)]
struct ChecklistItem {
    #[allow(dead_code)]
    id: String,
    text: String,
    #[serde(default)]
    completed: bool,
}

/// A task read from the persistent JSON file
#[derive(Debug, Clone, serde::Deserialize)]
struct Task {
    id: String,
    subject: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default, rename = "blockedBy")]
    blocked_by: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    owner: Option<String>,
    #[serde(default)]
    checklist: Vec<ChecklistItem>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// Metadata from meta.json
#[derive(Debug, serde::Deserialize)]
struct PersistMeta {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    session_id: String,
}

/// Compute project hash (must match `task_persist.rs`). Delegates to the shared
/// canonical implementation in `super::project_hash`.
fn project_hash(cwd: &str) -> String {
    super::project_hash(cwd)
}

/// Get the persistent tasks directory for a project (under
/// `~/.claude/sentinel/persistent-tasks/`).
fn persistent_tasks_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(super::persistent_tasks_root(&home).join(project_hash))
}

/// Read tasks from the persistent JSON file.
///
/// Missing tasks are normal for a fresh project and return `Ok(None)`.
/// Corrupt or unreadable task state is returned as an error so SessionStart can
/// inject authority context instead of treating data loss as an empty queue.
fn read_persistent_tasks(
    fs: &dyn FileSystemPort,
    project_hash: &str,
) -> Result<Option<Vec<Task>>, String> {
    let dir = persistent_tasks_dir(fs, project_hash)
        .ok_or_else(|| "cannot determine home directory for persisted tasks".to_string())?;
    let path = dir.join("tasks.json");
    if !fs.exists(&path) {
        return Ok(None);
    }
    let content = fs
        .read_to_string(&path)
        .map_err(|err| format!("failed to read persisted tasks {}: {err}", path.display()))?;
    serde_json::from_str::<Vec<Task>>(&content)
        .map(Some)
        .map_err(|err| format!("failed to parse persisted tasks {}: {err}", path.display()))
}

/// Read metadata
fn read_meta(fs: &dyn FileSystemPort, project_hash: &str) -> Result<Option<PersistMeta>, String> {
    let dir = persistent_tasks_dir(fs, project_hash)
        .ok_or_else(|| "cannot determine home directory for persisted task metadata".to_string())?;
    let path = dir.join("meta.json");
    if !fs.exists(&path) {
        return Ok(None);
    }
    let content = fs
        .read_to_string(&path)
        .map_err(|err| format!("failed to read task metadata {}: {err}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|err| format!("failed to parse task metadata {}: {err}", path.display()))
}

/// Check if the persisted tasks are from the CURRENT session.
/// If so, don't rehydrate — the tasks are already live in memory.
fn is_current_session(meta: &PersistMeta, current_session: &str) -> bool {
    meta.session_id == current_session
}

/// A cross-session task mirror this old is treated as abandoned: no live
/// session is plausibly still working it, so re-injecting its `in_progress`
/// tasks every SessionStart is noise. Matches the 14-day session-summary
/// age-out so the two horizons stay consistent.
const MAX_REHYDRATE_AGE_DAYS: i64 = 14;

/// True when a foreign-session snapshot's `updated_at` is older than the
/// rehydrate horizon. An unparseable timestamp is treated as NOT stale — we
/// never prune on ambiguous data, only on a clearly-old one.
fn is_stale_mirror(updated_at: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(updated_at).is_ok_and(|dt| {
        Utc::now().signed_duration_since(dt) > chrono::Duration::days(MAX_REHYDRATE_AGE_DAYS)
    })
}

/// Best-effort truncate of a stale cross-session mirror: blank `tasks.json` to
/// `[]` so it stops being rehydrated and stops rendering in the CLAUDE.md
/// Active Tasks table. Any IO failure is swallowed — GC must never break
/// SessionStart. (We deliberately do NOT delete the dir: keeping an empty
/// `tasks.json` preserves the same on-disk shape the live path maintains.)
fn truncate_stale_mirror(fs: &dyn FileSystemPort, project_hash: &str) {
    if let Some(dir) = persistent_tasks_dir(fs, project_hash) {
        let path = dir.join("tasks.json");
        let _ = fs.write(&path, b"[]");
    }
}

/// Format a human-readable relative time
fn relative_time(updated_at: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(updated_at) {
        let now = Utc::now();
        let diff = now.signed_duration_since(dt);
        if diff.num_minutes() < 1 {
            "just now".to_string()
        } else if diff.num_minutes() < 60 {
            format!("{}m ago", diff.num_minutes())
        } else if diff.num_hours() < 24 {
            format!("{}h ago", diff.num_hours())
        } else {
            format!("{}d ago", diff.num_days())
        }
    } else {
        updated_at.to_string()
    }
}

/// Format the rehydration instruction tail. Pure function so the
/// mode-aware branching can be unit-tested without a real `HookContext`.
///
/// In Autopilot the agent is meant to drain the queue, not interrupt the user
/// every session — instruction directs immediate recreation via `TaskCreate`.
/// In Planned mode, the user may have moved on from stale work, so instruction
/// directs the agent to ask first.
fn format_rehydrate_instruction(
    incomplete_count: usize,
    has_blocking: bool,
    autopilot: bool,
) -> String {
    if autopilot {
        if has_blocking {
            format!(
                "\n\nINSTRUCTION (AUTOPILOT — AUTO-REHYDRATE): Recreate these {incomplete_count} task(s) immediately \
                 using TaskCreate + TaskUpdate(addBlockedBy) to wire blocking chains exactly as shown. \
                 Preserve subjects, descriptions, status, and metadata verbatim. After rehydration, \
                 brief the user in one sentence (\"rehydrated N tasks\") and continue with their opening prompt."
            )
        } else {
            format!(
                "\n\nINSTRUCTION (AUTOPILOT — AUTO-REHYDRATE): Recreate these {incomplete_count} task(s) immediately \
                 using TaskCreate with the exact subjects, descriptions, status, and metadata shown above. \
                 After rehydration, brief the user in one sentence (\"rehydrated N tasks\") and continue \
                 with their opening prompt."
            )
        }
    } else if has_blocking {
        format!(
            "\n\nINSTRUCTION (PLANNED — ASK FIRST): Do NOT auto-recreate these tasks. \
             Ask the user: \"Found {incomplete_count} incomplete task(s) from a previous session — rehydrate them? (y/n)\". \
             If yes, recreate using TaskCreate + TaskUpdate(addBlockedBy) to wire blocking chains exactly as shown. \
             If no or unclear, skip rehydration and proceed with the user's opening prompt."
        )
    } else {
        format!(
            "\n\nINSTRUCTION (PLANNED — ASK FIRST): Do NOT auto-recreate these tasks. \
             Ask the user: \"Found {incomplete_count} incomplete task(s) from a previous session — rehydrate them? (y/n)\". \
             If yes, recreate using TaskCreate with the exact subjects and descriptions shown above. \
             If no or unclear, skip rehydration and proceed with the user's opening prompt."
        )
    }
}

/// Process `SessionStart` — inject persistent tasks + last-session prose.
///
/// Two independent context sources, either of which may be empty:
///   1. The structured task graph (`tasks.json`) — incomplete tasks to resume.
///   2. The prose `[Last Session]` summary (`session-summary.json`, written by
///      [`super::session_summary`]) — what shipped / was in flight last time.
///
/// We inject whichever exist. A session that finished all its tasks still gets
/// the prose summary ("last session you merged X, Y, Z"); a session killed
/// mid-work gets both.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let Some(session_id) = concrete_input_session_id(input) else {
        tracing::warn!("task_rehydrate skipped persisted context without concrete session id");
        return HookOutput::allow();
    };
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    let tasks_block = match build_tasks_block(input, ctx, &proj_hash, session_id) {
        Ok(block) => block,
        Err(err) => return authority_context(err),
    };
    let summary_block = match build_summary_block(ctx, &proj_hash, session_id) {
        Ok(block) => block,
        Err(err) => return super::session_summary::summary_read_error_context(err),
    };

    match (tasks_block, summary_block) {
        (None, None) => HookOutput::allow(),
        (Some(t), None) => HookOutput::inject_context(HookEvent::SessionStart, &t),
        (None, Some(s)) => HookOutput::inject_context(HookEvent::SessionStart, &s),
        (Some(t), Some(s)) => {
            // Prose summary first (orientation), then the structured task list.
            let combined = format!("{s}\n\n{t}");
            HookOutput::inject_context(HookEvent::SessionStart, &combined)
        }
    }
}

/// Build the structured persistent-tasks context block, or `None` when there's
/// nothing to rehydrate (no file, current-session tasks, or no incomplete tasks).
fn build_tasks_block(
    input: &HookInput,
    ctx: &HookContext<'_>,
    proj_hash: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    // Read persistent tasks
    let tasks = match read_persistent_tasks(ctx.fs, proj_hash) {
        Ok(Some(tasks)) if !tasks.is_empty() => tasks,
        Ok(_) => return Ok(None),
        Err(err) => return Err(err),
    };

    // Check if these are from the current session (skip rehydration)
    let meta = read_meta(ctx.fs, proj_hash)?;
    if let Some(meta) = &meta {
        if is_current_session(meta, session_id) {
            tracing::debug!("Persistent tasks are from current session — skipping rehydration");
            return Ok(None);
        }

        // SessionStart GC (the missing reconcile safety net): the mirror is
        // keyed per project-hash but the native task list is session-global, so
        // a session that cd's across repos can leave frozen `in_progress`
        // snapshots in hashes it never revisits. `task_persist`'s per-write
        // cleanup only fires while that session is live; once it ends, nothing
        // prunes the orphan — and this very function would otherwise re-inject
        // it into EVERY future session forever. So: if a foreign session's
        // snapshot is older than the rehydrate horizon, treat it as abandoned —
        // truncate it and skip injection, instead of nagging about tasks no
        // live session is working. Fresh foreign snapshots (a genuine recent
        // prior session) are still rehydrated as before.
        if is_stale_mirror(&meta.updated_at) {
            tracing::info!(
                hash = proj_hash,
                updated_at = %meta.updated_at,
                "Pruning stale cross-session task mirror (older than rehydrate horizon)"
            );
            truncate_stale_mirror(ctx.fs, proj_hash);
            return Ok(None);
        }
    }

    // Separate incomplete and completed
    let incomplete: Vec<&Task> = tasks.iter().filter(|t| t.status != "completed").collect();
    let completed_count = tasks.iter().filter(|t| t.status == "completed").count();

    if incomplete.is_empty() {
        return Ok(None);
    }

    let _ = input; // reserved for future per-input shaping
    let time_str = meta
        .as_ref()
        .map_or_else(|| "unknown".to_string(), |m| relative_time(&m.updated_at));

    // Detect whether any task has blocking relationships
    let has_blocking = incomplete
        .iter()
        .any(|t| !t.blocks.is_empty() || !t.blocked_by.is_empty());

    // Build context injection
    let mut context = format!(
        "📌 [Persistent Tasks] {} incomplete task(s) from previous session (updated {time_str}):\n",
        incomplete.len()
    );

    for task in &incomplete {
        let status_icon = match task.status.as_str() {
            "in_progress" => "🔄",
            _ => "⏳",
        };
        let _ = write!(
            context,
            "\n#{} [{status_icon}] {} ({})",
            task.id, task.subject, task.status
        );
        if !task.blocks.is_empty() {
            let _ = write!(context, " [blocks: {}]", task.blocks.join(", "));
        }
        if !task.blocked_by.is_empty() {
            let _ = write!(context, " [blocked by: {}]", task.blocked_by.join(", "));
        }
        // Render metadata inline
        if let Some(meta) = &task.metadata {
            if let Some(obj) = meta.as_object() {
                let mut meta_parts = Vec::new();
                if let Some(priority) = obj.get("priority").and_then(|v| v.as_str()) {
                    meta_parts.push(format!("priority={priority}"));
                }
                if let Some(phase) = obj.get("phase").and_then(|v| v.as_str()) {
                    meta_parts.push(format!("phase={phase}"));
                }
                if !meta_parts.is_empty() {
                    let _ = write!(context, " [{}]", meta_parts.join(", "));
                }
            }
        }
        if !task.description.is_empty() {
            // Truncate long descriptions
            let desc = if task.description.len() > 200 {
                format!("{}...", &task.description[..197])
            } else {
                task.description.clone()
            };
            let _ = write!(context, "\n  {desc}");
        }
        // Render checklist progress
        if !task.checklist.is_empty() {
            let done = task.checklist.iter().filter(|c| c.completed).count();
            let _ = write!(
                context,
                "\n  Checklist ({}/{}): {}",
                done,
                task.checklist.len(),
                task.checklist
                    .iter()
                    .map(|c| {
                        let mark = if c.completed { "x" } else { " " };
                        format!("[{mark}] {}", c.text)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    if completed_count > 0 {
        let _ = write!(
            context,
            "\n\n({completed_count} completed task(s) from previous session)"
        );
    }

    let instruction =
        format_rehydrate_instruction(incomplete.len(), has_blocking, ctx.autopilot_enabled());
    context.push_str(&instruction);

    Ok(Some(context))
}

fn authority_context(message: impl Into<String>) -> HookOutput {
    HookOutput::inject_context(
        HookEvent::SessionStart,
        format!(
            "{}[Task Rehydrate] {}",
            HookOutput::SENTINEL_AUTHORITY_PREFIX,
            message.into()
        ),
    )
}

/// Build the `[Last Session]` prose block from `session-summary.json`, or
/// `None` when absent / from the current session / empty.
///
/// Mirrors the on-disk struct written by [`super::session_summary`] without a
/// hard dependency on its exact field set (extra fields are ignored).
fn build_summary_block(
    ctx: &HookContext<'_>,
    proj_hash: &str,
    session_id: &str,
) -> Result<Option<String>, String> {
    let Some(summary) = super::session_summary::read_summary(ctx.fs, proj_hash)? else {
        return Ok(None);
    };

    // Skip a summary written by THIS session (e.g. SessionStart fired twice) —
    // it would just echo our own teardown back at us.
    if summary.session_id == session_id {
        return Ok(None);
    }

    // Age out ancient summaries. Nothing rewrites session-summary.json until a
    // later SessionEnd finds non-empty tasks for this project, so a repo
    // touched once and left would surface "ended 19d ago … 1 in progress"
    // verbatim at every session start, forever. Unparseable timestamps are
    // treated as stale.
    const MAX_SUMMARY_AGE_DAYS: i64 = 14;
    if chrono::DateTime::parse_from_rfc3339(&summary.written_at)
        .ok()
        .is_none_or(|t| {
            chrono::Utc::now().signed_duration_since(t)
                > chrono::Duration::days(MAX_SUMMARY_AGE_DAYS)
        })
    {
        return Ok(None);
    }

    let when = relative_time(&summary.written_at);
    let mut block = format!("🕘 [Last Session] ended {when}");
    if !summary.branch.is_empty() {
        let _ = write!(block, " on `{}`", summary.branch);
    }
    if !summary.head_sha.is_empty() {
        let _ = write!(block, " @ {}", summary.head_sha);
    }
    let _ = write!(
        block,
        ".\nTasks last seen: {} completed, {} in progress, {} pending.",
        summary.completed, summary.in_progress, summary.pending
    );

    if !summary.recent_commits.is_empty() {
        block.push_str("\nRecently shipped (newest first):");
        for c in summary.recent_commits.iter().take(10) {
            let _ = write!(block, "\n  • {c}");
        }
    }
    block.push_str(
        "\n\nThis is orientation context from the prior session — not a directive. \
         Use it to resume where work left off; the structured task list (if any) below is authoritative for what to do next.",
    );

    Ok(Some(block))
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

    fn process_with_home(home: PathBuf, cwd: &Path, session_id: &str) -> HookOutput {
        let git = crate::hooks::test_support::StubGit;
        let fs = TestFs { home };
        let process_port = crate::hooks::test_support::StubProcess;
        let memory_mcp = crate::hooks::test_support::StubMemoryMcp;
        let env = crate::hooks::test_support::StubEnv::new();
        let ctx = crate::hooks::HookContext {
            git: &git,
            vector_store: None,
            fs: &fs,
            process: &process_port,
            llm: None,
            memory_mcp: &memory_mcp,
            env: &env,
            linear_lookup: None,
        };
        let input = HookInput {
            session_id: Some(session_id.to_string()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };

        process(&input, &ctx)
    }

    fn injected_context(output: &HookOutput) -> &str {
        output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref())
            .expect("expected context injection")
    }

    #[test]
    fn test_project_hash_matches_persist() {
        // Must match task_persist.rs hash
        let h = project_hash("/Users/operator/projects/firefly");
        assert_eq!(h.len(), 8);
    }

    #[test]
    fn test_relative_time() {
        // Valid RFC3339
        let recent = Utc::now().to_rfc3339();
        assert_eq!(relative_time(&recent), "just now");

        // Invalid string
        assert_eq!(relative_time("not-a-date"), "not-a-date");
    }

    #[test]
    fn test_is_current_session() {
        let meta = PersistMeta {
            updated_at: "2026-04-04T12:00:00Z".to_string(),
            session_id: "abc-123".to_string(),
        };
        assert!(is_current_session(&meta, "abc-123"));
        assert!(!is_current_session(&meta, "def-456"));
    }

    #[test]
    fn autopilot_instruction_directs_immediate_recreation() {
        let s = format_rehydrate_instruction(80, false, true);
        assert!(s.contains("AUTOPILOT"), "missing mode tag: {s}");
        assert!(s.contains("AUTO-REHYDRATE"), "missing action: {s}");
        assert!(s.contains("immediately"), "should say immediately: {s}");
        assert!(s.contains("80 task(s)"), "missing count: {s}");
        assert!(
            !s.contains("ASK FIRST"),
            "autopilot must not include ASK FIRST: {s}"
        );
    }

    #[test]
    fn autopilot_instruction_with_blocking_mentions_blockedby() {
        let s = format_rehydrate_instruction(5, true, true);
        assert!(s.contains("AUTOPILOT"), "missing mode tag: {s}");
        assert!(
            s.contains("addBlockedBy"),
            "blocking variant must direct addBlockedBy: {s}"
        );
    }

    #[test]
    fn planned_instruction_asks_first() {
        let s = format_rehydrate_instruction(80, false, false);
        assert!(s.contains("PLANNED"), "missing mode tag: {s}");
        assert!(s.contains("ASK FIRST"), "missing ask gate: {s}");
        assert!(s.contains("80 incomplete task(s)"), "missing count: {s}");
        assert!(
            !s.contains("AUTO-REHYDRATE"),
            "planned must not auto-rehydrate: {s}"
        );
    }

    #[test]
    fn planned_instruction_with_blocking_still_asks_first() {
        let s = format_rehydrate_instruction(5, true, false);
        assert!(s.contains("PLANNED"));
        assert!(s.contains("ASK FIRST"));
        assert!(s.contains("addBlockedBy"));
    }

    #[test]
    fn test_process_no_persistent_tasks() {
        let input = HookInput {
            session_id: Some("test-session".to_string()),
            cwd: Some("/nonexistent/project".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        // Should allow (no tasks to inject)
        assert!(
            output.hook_specific_output.is_none() || {
                output
                    .hook_specific_output
                    .as_ref()
                    .and_then(|h| h.additional_context.as_ref())
                    .is_none()
            }
        );
    }

    #[test]
    fn read_persistent_tasks_ignores_non_sentinel_legacy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let project = "legacy-only";
        let legacy_dir = home.join(".claude").join("persistent-tasks").join(project);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join("tasks.json"),
            r#"[{"id":"T-1","subject":"legacy","status":"pending"}]"#,
        )
        .unwrap();

        let fs = TestFs { home: home.clone() };
        let tasks = read_persistent_tasks(&fs, project).expect("canonical task read");

        assert!(tasks.is_none(), "legacy task snapshots must be ignored");
        assert!(
            !home
                .join(".claude")
                .join("sentinel")
                .join("persistent-tasks")
                .exists(),
            "legacy data must not be migrated into Sentinel authority state"
        );
    }

    #[test]
    fn summary_block_absent_yields_none() {
        // No session-summary.json on disk for this hash → no prose block.
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(build_summary_block(&ctx, "zzzzzzzz", "any-session")
            .unwrap()
            .is_none());
    }

    #[test]
    fn summary_block_ages_out_after_recency_window() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let dir = persistent_tasks_dir(&fs, "agedhash").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let write_summary = |written_at: &str| {
            std::fs::write(
                dir.join("session-summary.json"),
                format!(
                    r#"{{"session_id":"other-sess","written_at":"{written_at}","branch":"","head_sha":"","recent_commits":[],"completed":0,"in_progress":1,"pending":0}}"#
                ),
            )
            .unwrap();
        };

        // Ancient summary (the "ended 19d ago … 1 in progress" line that
        // nothing ever rewrites) → suppressed.
        write_summary("2026-06-01T00:00:00Z");
        assert!(
            build_summary_block(&ctx, "agedhash", "current-sess")
                .unwrap()
                .is_none(),
            "summaries beyond the recency window must not be replayed"
        );

        // Fresh summary → rendered.
        write_summary(&chrono::Utc::now().to_rfc3339());
        assert!(build_summary_block(&ctx, "agedhash", "current-sess")
            .unwrap()
            .is_some());
    }

    #[test]
    fn is_stale_mirror_discriminates_by_age() {
        // Well past the horizon → stale.
        assert!(is_stale_mirror("2026-01-01T00:00:00Z"));
        // Just now → not stale.
        assert!(!is_stale_mirror(&chrono::Utc::now().to_rfc3339()));
        // Unparseable → never treated as stale (no prune on ambiguous data).
        assert!(!is_stale_mirror("not-a-timestamp"));
        assert!(!is_stale_mirror(""));
    }

    #[test]
    fn stale_cross_session_mirror_is_pruned_and_not_rehydrated() {
        // The orphan-ghost end state: a foreign session left an in_progress
        // snapshot that is now older than the rehydrate horizon. SessionStart
        // must NOT re-inject it, and must truncate it so it stops rendering in
        // the Active Tasks table forever.
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let hash = "ghosthash";
        let dir = persistent_tasks_dir(&fs, hash).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tasks.json"),
            r#"[{"id":"1","subject":"orphaned ghost","status":"in_progress","blocks":[],"blocked_by":[]}]"#,
        )
        .unwrap();
        // Foreign session, ancient timestamp.
        std::fs::write(
            dir.join("meta.json"),
            r#"{"project_hash":"ghosthash","cwd":"/old/repo","session_id":"long-dead-session","updated_at":"2026-01-01T00:00:00Z","task_count":1,"incomplete_count":1,"last_block_hash":""}"#,
        )
        .unwrap();

        let input = HookInput {
            session_id: Some("current-session".to_string()),
            cwd: Some("/whatever".to_string()),
            ..Default::default()
        };
        // build_tasks_block reads the mirror for project_hash(cwd); force the
        // ghost hash by asserting the helper directly (cwd hashing is covered
        // elsewhere). Prune + skip:
        let block = build_tasks_block(&input, &ctx, hash, "current-session").unwrap();
        assert!(
            block.is_none(),
            "a stale cross-session mirror must not be rehydrated"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("tasks.json"))
                .unwrap()
                .trim(),
            "[]",
            "the stale mirror must be truncated so it stops re-injecting/rendering"
        );
    }

    #[test]
    fn fresh_cross_session_mirror_is_still_rehydrated() {
        // Guard against over-pruning: a RECENT foreign-session snapshot (a
        // genuine prior session moments ago) must still rehydrate as before.
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let hash = "freshhash";
        let dir = persistent_tasks_dir(&fs, hash).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("tasks.json"),
            r#"[{"id":"1","subject":"real recent task","status":"in_progress","blocks":[],"blocked_by":[]}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"project_hash":"freshhash","cwd":"/r","session_id":"prev-session","updated_at":"{}","task_count":1,"incomplete_count":1,"last_block_hash":""}}"#,
                chrono::Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

        let input = HookInput {
            session_id: Some("current-session".to_string()),
            cwd: Some("/whatever".to_string()),
            ..Default::default()
        };
        let block = build_tasks_block(&input, &ctx, hash, "current-session").unwrap();
        assert!(
            block.is_some(),
            "a fresh cross-session mirror must still be rehydrated"
        );
        assert!(
            block.unwrap().contains("real recent task"),
            "the rehydrated block should name the incomplete task"
        );
    }

    #[test]
    fn no_tasks_and_no_summary_allows_quietly() {
        // The combined path: neither source present → plain allow(), no context.
        let input = HookInput {
            session_id: Some("test-session".to_string()),
            cwd: Some("/nonexistent/project".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        let has_ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_ref())
            .is_some();
        assert!(
            !has_ctx,
            "expected no context injection when both sources empty"
        );
    }

    #[test]
    fn missing_session_does_not_read_persisted_tasks_or_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());
        let task_dir = persistent_tasks_dir(&fs, &project).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("tasks.json"), "not-json").unwrap();
        let summary_path =
            super::super::session_summary::summary_path(&fs, &project).expect("summary path");
        std::fs::write(summary_path, "not-json").unwrap();

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
            "missing session must not consume or report persisted unknown-session context"
        );
    }

    #[test]
    fn synthetic_session_does_not_rehydrate_unknown_context() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());
        let task_dir = persistent_tasks_dir(&fs, &project).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(
            task_dir.join("tasks.json"),
            r#"[{"id":"T-1","subject":"should not rehydrate","status":"pending"}]"#,
        )
        .unwrap();
        std::fs::write(
            task_dir.join("meta.json"),
            r#"{"session_id":"previous-real-session","updated_at":"2026-05-30T00:00:00Z"}"#,
        )
        .unwrap();

        let input = HookInput {
            session_id: Some(" unknown ".to_string()),
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
            "synthetic session must not rehydrate persisted task state"
        );
    }

    #[test]
    fn corrupt_session_summary_injects_authority_context() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());
        let summary_path =
            super::super::session_summary::summary_path(&TestFs { home: home.clone() }, &project)
                .unwrap();
        std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
        std::fs::write(&summary_path, "not-json").unwrap();

        let git = crate::hooks::test_support::StubGit;
        let fs = TestFs { home };
        let process_port = crate::hooks::test_support::StubProcess;
        let memory_mcp = crate::hooks::test_support::StubMemoryMcp;
        let env = crate::hooks::test_support::StubEnv::new();
        let ctx = crate::hooks::HookContext {
            git: &git,
            vector_store: None,
            fs: &fs,
            process: &process_port,
            llm: None,
            memory_mcp: &memory_mcp,
            env: &env,
            linear_lookup: None,
        };
        let input = HookInput {
            session_id: Some("new-session".to_string()),
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let output = process(&input, &ctx);
        let context = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref())
            .expect("corrupt summary must be visible");
        assert!(context.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(context.contains("failed to parse session summary"));
    }

    #[test]
    fn corrupt_tasks_json_injects_authority_context() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());
        let fs = TestFs { home: home.clone() };
        let task_dir = persistent_tasks_dir(&fs, &project).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("tasks.json"), "not-json").unwrap();

        let output = process_with_home(home, &cwd, "new-session");
        let context = injected_context(&output);
        assert!(context.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(context.contains("[Task Rehydrate]"));
        assert!(context.contains("failed to parse persisted tasks"));
    }

    #[test]
    fn corrupt_meta_json_injects_authority_context() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = project_hash(cwd.to_str().unwrap());
        let fs = TestFs { home: home.clone() };
        let task_dir = persistent_tasks_dir(&fs, &project).unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(
            task_dir.join("tasks.json"),
            r#"[{"id":"T-1","subject":"Resume enterprise path","status":"pending"}]"#,
        )
        .unwrap();
        std::fs::write(task_dir.join("meta.json"), "not-json").unwrap();

        let output = process_with_home(home, &cwd, "new-session");
        let context = injected_context(&output);
        assert!(context.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(context.contains("[Task Rehydrate]"));
        assert!(context.contains("failed to parse task metadata"));
    }
}
