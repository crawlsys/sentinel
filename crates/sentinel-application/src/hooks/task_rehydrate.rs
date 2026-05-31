//! Task Rehydrate Hook — inject persistent tasks on `SessionStart`
//!
//! Fires on `SessionStart`. Reads `~/.claude/sentinel/persistent-tasks/{project_hash}/tasks.json`
//! and injects incomplete tasks into context as a system reminder so Claude
//! sees prior work and can continue where the previous session left off.
//! (Legacy `~/.claude/persistent-tasks/` data is migrated automatically on
//! first read — see `super::migrate_persistent_tasks_dir`.)
//!
//! Only injects tasks that are NOT completed — completed tasks are mentioned
//! as a summary count but not listed in full.

use chrono::Utc;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::fmt::Write as _;
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

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
///
/// Triggers a one-time migration from the legacy `~/.claude/persistent-tasks/`
/// path the first time it's called per process.
fn persistent_tasks_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    super::migrate_persistent_tasks_dir(fs, &home);
    Some(super::persistent_tasks_root(&home).join(project_hash))
}

/// Read tasks from the persistent JSON file.
///
/// **Bug fix (2026-05-06)**: Previously used `serde_json::from_str(&content).ok()?`
/// which silently swallowed parse errors and returned None. If tasks.json was
/// ever corrupted (half-written from a non-atomic write, malformed by an
/// external editor, disk error), the rehydrator would treat it as "no tasks"
/// and inject nothing — silent data loss with zero warning to the user.
///
/// The companion fix in `task_persist.rs` makes the write atomic, but we still
/// want defense in depth: if a parse fails for any reason (concurrent writer,
/// disk issue, manual edit gone wrong), log a loud warning so the user knows
/// their persistent task store is corrupt. They can then recover from the
/// per-session task dir at ~/.claude/tasks/<session>/ before it's too late.
fn read_persistent_tasks(fs: &dyn FileSystemPort, project_hash: &str) -> Option<Vec<Task>> {
    let dir = persistent_tasks_dir(fs, project_hash)?;
    let path = dir.join("tasks.json");
    let content = match fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return None, // file simply doesn't exist yet — quiet path
    };
    match serde_json::from_str::<Vec<Task>>(&content) {
        Ok(tasks) => Some(tasks),
        Err(e) => {
            tracing::warn!(
                project_hash = project_hash,
                path = %path.display(),
                error = %e,
                content_len = content.len(),
                "CORRUPT tasks.json detected during rehydration — refusing to \
                 silently treat as empty. User's tasks may be in a half-written \
                 state. Recover from ~/.claude/tasks/<session>/ if available."
            );
            // Print to stderr too so it surfaces in the SessionStart hook output
            // even when tracing is misconfigured.
            eprintln!(
                "[sentinel] task_rehydrate: corrupt tasks.json at {} — {} bytes, \
                 parse error: {}. Tasks NOT rehydrated. Investigate before \
                 creating new tasks (they may overlap or collide).",
                path.display(),
                content.len(),
                e
            );
            None
        }
    }
}

/// Read metadata
fn read_meta(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PersistMeta> {
    let dir = persistent_tasks_dir(fs, project_hash)?;
    let path = dir.join("meta.json");
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Check if the persisted tasks are from the CURRENT session.
/// If so, don't rehydrate — the tasks are already live in memory.
fn is_current_session(meta: &PersistMeta, current_session: &str) -> bool {
    meta.session_id == current_session
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
/// In Autopilot the agent is meant to drain the queue, not interrupt Gary
/// every session — instruction directs immediate recreation via `TaskCreate`.
/// In Planned mode, Gary may have moved on from stale work, so instruction
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
                 brief Gary in one sentence (\"rehydrated N tasks\") and continue with his opening prompt."
            )
        } else {
            format!(
                "\n\nINSTRUCTION (AUTOPILOT — AUTO-REHYDRATE): Recreate these {incomplete_count} task(s) immediately \
                 using TaskCreate with the exact subjects, descriptions, status, and metadata shown above. \
                 After rehydration, brief Gary in one sentence (\"rehydrated N tasks\") and continue \
                 with his opening prompt."
            )
        }
    } else if has_blocking {
        format!(
            "\n\nINSTRUCTION (PLANNED — ASK FIRST): Do NOT auto-recreate these tasks. \
             Ask Gary: \"Found {incomplete_count} incomplete task(s) from a previous session — rehydrate them? (y/n)\". \
             If yes, recreate using TaskCreate + TaskUpdate(addBlockedBy) to wire blocking chains exactly as shown. \
             If no or unclear, skip silently and proceed with the user's opening prompt."
        )
    } else {
        format!(
            "\n\nINSTRUCTION (PLANNED — ASK FIRST): Do NOT auto-recreate these tasks. \
             Ask Gary: \"Found {incomplete_count} incomplete task(s) from a previous session — rehydrate them? (y/n)\". \
             If yes, recreate using TaskCreate with the exact subjects and descriptions shown above. \
             If no or unclear, skip silently and proceed with the user's opening prompt."
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
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    let tasks_block = build_tasks_block(input, ctx, &proj_hash, session_id);
    let summary_block = build_summary_block(ctx, &proj_hash, session_id);

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
) -> Option<String> {
    // Read persistent tasks
    let tasks = match read_persistent_tasks(ctx.fs, proj_hash) {
        Some(t) if !t.is_empty() => t,
        _ => return None,
    };

    // Check if these are from the current session (skip rehydration)
    if let Some(meta) = read_meta(ctx.fs, proj_hash) {
        if is_current_session(&meta, session_id) {
            tracing::debug!("Persistent tasks are from current session — skipping rehydration");
            return None;
        }
    }

    // Separate incomplete and completed
    let incomplete: Vec<&Task> = tasks.iter().filter(|t| t.status != "completed").collect();
    let completed_count = tasks.iter().filter(|t| t.status == "completed").count();

    if incomplete.is_empty() {
        return None;
    }

    let _ = input; // reserved for future per-input shaping
    // Read meta for timestamp
    let time_str = read_meta(ctx.fs, proj_hash).map_or_else(|| "unknown".to_string(), |m| relative_time(&m.updated_at));

    // Detect whether any task has blocking relationships
    let has_blocking = incomplete
        .iter()
        .any(|t| !t.blocks.is_empty() || !t.blocked_by.is_empty());

    // Build context injection
    let mut context = format!(
        "[Persistent Tasks] {} incomplete task(s) from previous session (updated {time_str}):\n",
        incomplete.len()
    );

    for task in &incomplete {
        let status_icon = match task.status.as_str() {
            "in_progress" => "~",
            _ => " ",
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

    Some(context)
}

/// Build the `[Last Session]` prose block from `session-summary.json`, or
/// `None` when absent / from the current session / empty.
///
/// Mirrors the on-disk struct written by [`super::session_summary`] without a
/// hard dependency on its exact field set (extra fields are ignored).
fn build_summary_block(ctx: &HookContext<'_>, proj_hash: &str, session_id: &str) -> Option<String> {
    let summary = super::session_summary::read_summary(ctx.fs, proj_hash)?;

    // Skip a summary written by THIS session (e.g. SessionStart fired twice) —
    // it would just echo our own teardown back at us.
    if summary.session_id == session_id {
        return None;
    }

    let when = relative_time(&summary.written_at);
    let mut block = format!("[Last Session] ended {when}");
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

    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_hash_matches_persist() {
        // Must match task_persist.rs hash
        let h = project_hash("/Users/gary/projects/firefly");
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
    fn summary_block_absent_yields_none() {
        // No session-summary.json on disk for this hash → no prose block.
        let ctx = crate::hooks::test_support::stub_ctx();
        assert!(build_summary_block(&ctx, "zzzzzzzz", "any-session").is_none());
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
        assert!(!has_ctx, "expected no context injection when both sources empty");
    }
}
