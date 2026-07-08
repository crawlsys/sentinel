//! Task Status Line Hook — tasks as first-class citizens.
//!
//! Runs on **every** `UserPromptSubmit`. Reads the live native TaskList for the
//! current session (`~/.claude/tasks/{session_id}/` or the `session-{group}`
//! variant, resolved by [`super::session_task_dir`]) and injects an emoji task
//! block so the operator's in-flight work is visible on every turn:
//!
//! ```text
//! 📋 [Tasks] sentinel — 2 pending, 1 in progress, 8 done
//!   🔄 #34 Make tasks first-class: every-turn TaskList emoji status line
//!   ⏳ #35 Wire the next thing
//! ```
//!
//! Unlike `todo_loader` (which reads the `TodoWrite` store `active.jsonl` and
//! loads once per session), this hook reads the durable **TaskList** the
//! operator's CLAUDE.md mandates, and fires **every turn** — the queue is a
//! first-class citizen, not a one-shot banner.
//!
//! Design guarantees:
//! - **Session-scoped**: only THIS session's task dir is read, so the output is
//!   small and relevant (no cross-session graveyard — the `todo_loader` 4451
//!   lesson).
//! - **Silent when empty**: no active tasks → inject nothing (never spam an
//!   empty line every turn).
//! - **Robust**: unreadable dir / malformed files → silent; never panics;
//!   subject truncation is char-boundary-safe.
//! - **Canonical emoji**: status glyphs come from
//!   [`sentinel_domain::task_decoration::status_glyph`] — the single source of
//!   truth shared with the CLAUDE.md Active Tasks table.
//!
//! All IO goes through `ctx.fs` (`FileSystemPort`).

use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::task_decoration::status_glyph;

use super::{concrete_input_session_id, FileSystemPort, HookContext};

/// Max active tasks rendered as individual lines; the rest are summarised as
/// `+N more`. Keeps a very long queue from dominating every message.
const MAX_LISTED: usize = 10;

/// Subject text is truncated to this many CHARACTERS (not bytes — multibyte
/// safe) so one verbose task can't blow out the line width.
const MAX_SUBJECT_CHARS: usize = 80;

/// A single native TaskList entry. Deliberately minimal — only the fields this
/// line needs — so it deserializes any TaskList row shape.
#[derive(Debug, serde::Deserialize)]
struct TaskRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    status: String,
}

/// Resolve this session's native TaskList directory (literal `{session_id}` or
/// the `session-{group}` variant), or `None` when the session id is absent.
fn task_dir(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(super::session_task_dir(fs, &home, session_id))
}

/// Read every `*.json` task file in `dir` (skipping dotfiles / non-json), sorted
/// by numeric id. Any read/parse failure is skipped, not fatal.
fn read_tasks(fs: &dyn FileSystemPort, dir: &Path) -> Vec<TaskRow> {
    let mut tasks = Vec::new();
    let Ok(entries) = fs.read_dir(dir) else {
        return tasks;
    };
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with('.')
            || !Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        if let Ok(content) = fs.read_to_string(&path) {
            if let Ok(task) = serde_json::from_str::<TaskRow>(&content) {
                if !task.id.is_empty() {
                    tasks.push(task);
                }
            }
        }
    }
    tasks.sort_by(|a, b| {
        let a_num: u32 = a.id.parse().unwrap_or(u32::MAX);
        let b_num: u32 = b.id.parse().unwrap_or(u32::MAX);
        a_num.cmp(&b_num).then_with(|| a.id.cmp(&b.id))
    });
    tasks
}

fn is_active(status: &str) -> bool {
    status == "in_progress" || status == "pending"
}

/// Truncate a subject to `MAX_SUBJECT_CHARS` on a char boundary, appending `…`
/// when it was cut. `&s[..n]` would panic mid-multibyte — take chars instead.
fn truncate_subject(subject: &str) -> String {
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        format!(
            "{}…",
            subject.chars().take(MAX_SUBJECT_CHARS).collect::<String>()
        )
    } else {
        subject.to_string()
    }
}

/// Build the injected task block from the current task set, or `None` when
/// there is nothing active to show. Pure so it can be unit-tested directly.
fn render_block(tasks: &[TaskRow], project_name: &str) -> Option<String> {
    let pending = tasks.iter().filter(|t| t.status == "pending").count();
    let in_progress = tasks
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();
    let done = tasks.iter().filter(|t| t.status == "completed").count();

    // In-progress first (what you're doing now), then pending (what's next).
    let mut active: Vec<&TaskRow> = tasks.iter().filter(|t| is_active(&t.status)).collect();
    if active.is_empty() {
        return None;
    }
    active.sort_by_key(|t| match t.status.as_str() {
        "in_progress" => 0,
        _ => 1,
    });

    use std::fmt::Write as _;
    let mut block = format!(
        "📋 [Tasks] {project_name} — {pending} pending, {in_progress} in progress, {done} done"
    );
    for t in active.iter().take(MAX_LISTED) {
        let glyph = status_glyph(&t.status).unwrap_or("•");
        let _ = write!(
            block,
            "\n  {glyph} #{} {}",
            t.id,
            truncate_subject(&t.subject)
        );
    }
    let overflow = active.len().saturating_sub(MAX_LISTED);
    if overflow > 0 {
        let _ = write!(block, "\n  …and {overflow} more active");
    }
    Some(block)
}

/// Process the task-status-line hook event. Fires every UserPromptSubmit.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // No concrete session id → we can't scope to a session's tasks; stay quiet
    // rather than reading some other session's dir.
    let Some(session_id) = concrete_input_session_id(input) else {
        return HookOutput::allow();
    };
    let Some(dir) = task_dir(ctx.fs, session_id) else {
        return HookOutput::allow();
    };
    if !ctx.fs.is_dir(&dir) {
        return HookOutput::allow();
    }

    let tasks = read_tasks(ctx.fs, &dir);

    let cwd = input.cwd.as_deref().unwrap_or(".");
    let project_name = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");

    match render_block(&tasks, project_name) {
        Some(block) => HookOutput::inject_context(HookEvent::UserPromptSubmit, block),
        None => HookOutput::allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    fn row(id: &str, status: &str, subject: &str) -> TaskRow {
        TaskRow {
            id: id.into(),
            status: status.into(),
            subject: subject.into(),
        }
    }

    #[test]
    fn render_lists_active_tasks_with_canonical_glyphs() {
        let tasks = vec![
            row("1", "completed", "old done thing"),
            row("2", "pending", "next up"),
            row("33", "in_progress", "the current work"),
        ];
        let block = render_block(&tasks, "sentinel").expect("active tasks → block");
        // Summary counts.
        assert!(
            block.contains("1 pending, 1 in progress, 1 done"),
            "{block}"
        );
        // In-progress rendered first, with the canonical 🔄 glyph + id + subject.
        assert!(block.contains("🔄 #33 the current work"), "{block}");
        assert!(block.contains("⏳ #2 next up"), "{block}");
        // Completed tasks are counted but not listed as active lines.
        assert!(!block.contains("old done thing"), "{block}");
        // Ordering: the in_progress line precedes the pending line.
        let ip = block.find("🔄 #33").unwrap();
        let pd = block.find("⏳ #2").unwrap();
        assert!(ip < pd, "in_progress must be listed before pending: {block}");
    }

    #[test]
    fn empty_or_all_done_renders_nothing() {
        // No tasks at all.
        assert!(render_block(&[], "sentinel").is_none());
        // Only completed → nothing active to show.
        let done = vec![row("1", "completed", "done")];
        assert!(
            render_block(&done, "sentinel").is_none(),
            "all-done queue must stay silent"
        );
    }

    #[test]
    fn subject_truncation_is_char_boundary_safe() {
        // A multibyte subject longer than the cap must not panic and must be
        // truncated with an ellipsis. Box-drawing chars are 3 bytes each.
        let long = "│".repeat(200);
        let tasks = vec![row("1", "in_progress", &long)];
        let block = render_block(&tasks, "p").expect("block");
        assert!(block.contains('…'), "long subject should be ellipsised");
        // The rendered subject must be exactly MAX_SUBJECT_CHARS chars + '…'.
        let line = block.lines().find(|l| l.contains("#1")).unwrap();
        let subj_chars = line.chars().filter(|&c| c == '│').count();
        assert_eq!(subj_chars, MAX_SUBJECT_CHARS, "truncated to the char cap");
    }

    #[test]
    fn overflow_summarised_after_cap() {
        let tasks: Vec<TaskRow> = (1..=15)
            .map(|i| row(&i.to_string(), "pending", &format!("task {i}")))
            .collect();
        let block = render_block(&tasks, "p").expect("block");
        let listed = block.matches("⏳ #").count();
        assert_eq!(listed, MAX_LISTED, "only the cap is listed individually");
        assert!(block.contains("…and 5 more active"), "{block}");
    }

    #[test]
    fn process_fires_every_turn_no_one_shot_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        // Seed a native task dir for the session with one in_progress task.
        let sid = "tsl-sess-1";
        let dir = tmp.path().join(".claude").join("tasks").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"live task","status":"in_progress"}"#,
        )
        .unwrap();

        let input = HookInput {
            session_id: Some(sid.to_string()),
            cwd: Some("/repo/sentinel".to_string()),
            ..Default::default()
        };

        // Fire twice — both must inject (no one-shot suppression).
        for _ in 0..2 {
            let out = process(&input, &ctx);
            let ctx_str = out
                .hook_specific_output
                .and_then(|h| h.additional_context)
                .expect("task line must inject every turn");
            assert!(ctx_str.contains("📋 [Tasks] sentinel"), "{ctx_str}");
            assert!(ctx_str.contains("🔄 #1 live task"), "{ctx_str}");
        }
    }

    #[test]
    fn process_silent_when_no_task_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let input = HookInput {
            session_id: Some("tsl-no-dir".to_string()),
            cwd: Some("/repo/x".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(
            out.hook_specific_output.is_none(),
            "a session with no task dir must inject nothing"
        );
    }

    #[test]
    fn process_reads_session_prefixed_dir_variant() {
        // Claude Code sometimes writes the native dir as `session-{group}`
        // rather than the full session id. session_task_dir resolves both;
        // confirm the line reads the prefixed variant.
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let sid = "abcd1234-ef00-0000-0000-000000000000";
        let prefixed = tmp
            .path()
            .join(".claude")
            .join("tasks")
            .join("session-abcd1234");
        std::fs::create_dir_all(&prefixed).unwrap();
        std::fs::write(
            prefixed.join("7.json"),
            r#"{"id":"7","subject":"prefixed dir task","status":"pending"}"#,
        )
        .unwrap();

        let input = HookInput {
            session_id: Some(sid.to_string()),
            cwd: Some("/r/proj".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        let ctx_str = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("must read the session-prefixed dir");
        assert!(ctx_str.contains("⏳ #7 prefixed dir task"), "{ctx_str}");
    }

    #[test]
    fn process_returns_allow_on_missing_session() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&input, &ctx);
        assert!(out.blocked.is_none());
        assert!(out.hook_specific_output.is_none());
    }
}
