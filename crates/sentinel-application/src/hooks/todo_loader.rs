//! Todo Loader Hook
//!
//! Runs on `UserPromptSubmit`. Reads persistent todos from
//! `~/.claude/todos/active.jsonl`, filters to the current PROJECT — exact
//! normalized-path or exact-basename match, never substring — splits counts
//! between THIS session and other recent sessions, and injects a one-line
//! emoji summary. Only loads once per session (uses a temp marker file).
//!
//! All IO goes through `ctx.fs` (`FileSystemPort`).

use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{concrete_input_session_id, FileSystemPort, HookContext};

/// Rows from other sessions older than this are counted as stale and hidden
/// from the summary — dead sessions never clean up after themselves, so the
/// store accumulates thousands of rows that would otherwise drown the line.
const RECENT_WINDOW_DAYS: i64 = 14;

/// A single todo entry from active.jsonl
#[derive(Debug, serde::Deserialize)]
struct TodoEntry {
    #[serde(default)]
    content: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    priority: Option<u8>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

fn todos_file(fs: &dyn FileSystemPort) -> PathBuf {
    fs.claude_dir().join("todos").join("active.jsonl")
}

fn session_marker(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-todo-loaded-{session_id}"))
}

/// Read and parse all todos from a file
fn read_todos(fs: &dyn FileSystemPort, path: &Path) -> Vec<TodoEntry> {
    let content = match fs.read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<TodoEntry>(line).ok())
        .collect()
}

/// Exact project match: normalized full-path equality, or exact basename
/// equality (case-insensitive).
///
/// NEVER substring — the previous `proj.contains(project_name)` made a
/// home-dir session (`project_name = "garys"`) match every row whose project
/// path lives under `C:\Users\garys`, i.e. the entire store: the infamous
/// `[Todos] Pending: 4451` line.
fn project_matches(proj: &str, cwd: &str, cwd_basename: &str) -> bool {
    if super::normalize_path(proj) == super::normalize_path(cwd) {
        return true;
    }
    Path::new(proj)
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|b| b.eq_ignore_ascii_case(cwd_basename))
}

/// Filter todos relevant to the current project
fn filter_project_todos<'a>(todos: &'a [TodoEntry], cwd: &str) -> Vec<&'a TodoEntry> {
    let cwd_basename = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    todos
        .iter()
        .filter(|t| {
            t.project
                .as_deref()
                .is_some_and(|proj| project_matches(proj, cwd, cwd_basename))
        })
        .collect()
}

/// True when the row was touched within the recent window.
fn is_recent(updated_at: Option<&str>) -> bool {
    updated_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .is_some_and(|t| {
            chrono::Utc::now().signed_duration_since(t)
                <= chrono::Duration::days(RECENT_WINDOW_DAYS)
        })
}

fn priority_emoji(priority: Option<u8>) -> &'static str {
    match priority {
        Some(0) => "🔴",
        Some(1) => "🟠",
        Some(2) => "🟡",
        _ => "🟢",
    }
}

fn is_active(t: &TodoEntry) -> bool {
    t.status == "pending" || t.status == "in_progress"
}

/// Process the todo-loader hook event
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let session_id = concrete_input_session_id(input);

    // Check session marker — only load once per session
    if let Some(session_id) = session_id {
        let marker = session_marker(session_id);
        if ctx.fs.exists(&marker) {
            return HookOutput::allow();
        }
    }

    let todos_path = todos_file(ctx.fs);
    let all_todos = read_todos(ctx.fs, &todos_path);

    if all_todos.is_empty() {
        write_session_marker(ctx.fs, session_id);
        return HookOutput::allow();
    }

    // Active rows for THIS project only.
    let active: Vec<&TodoEntry> = filter_project_todos(&all_todos, cwd)
        .into_iter()
        .filter(|t| is_active(t))
        .collect();

    // Split: this session's rows / other sessions' RECENT rows. Everything
    // else is a stale remnant of a dead session — hidden, never counted into
    // the headline numbers.
    let (mine, other): (Vec<&TodoEntry>, Vec<&TodoEntry>) = active
        .iter()
        .partition(|t| t.session_id.as_deref() == session_id && session_id.is_some());
    let recent_other: Vec<&&TodoEntry> = other
        .iter()
        .filter(|t| is_recent(t.updated_at.as_deref()))
        .collect();

    let mine_pending = mine.iter().filter(|t| t.status == "pending").count();
    let mine_in_progress = mine.iter().filter(|t| t.status == "in_progress").count();
    let recent_pending = recent_other
        .iter()
        .filter(|t| t.status == "pending")
        .count();
    let recent_in_progress = recent_other
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();

    if mine.is_empty() && recent_other.is_empty() {
        // Nothing live for this project — stay silent (a wall of stale rows
        // from dead sessions is noise, not a todo list).
        write_session_marker(ctx.fs, session_id);
        return HookOutput::allow();
    }

    let p0_count = mine
        .iter()
        .chain(recent_other.iter().copied())
        .filter(|t| t.priority == Some(0))
        .count();
    let p1_count = mine
        .iter()
        .chain(recent_other.iter().copied())
        .filter(|t| t.priority == Some(1))
        .count();

    // Top 3, this session's rows first, then recent ones, by priority.
    let mut ranked: Vec<&TodoEntry> = mine
        .iter()
        .chain(recent_other.iter().copied())
        .copied()
        .collect();
    ranked.sort_by_key(|t| t.priority.unwrap_or(2));
    let top_todos: Vec<String> = ranked
        .iter()
        .take(3)
        .map(|t| format!("{} {}", priority_emoji(t.priority), t.content))
        .collect();

    let project_name = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let stale_hidden = other.len() - recent_other.len();
    let mut context = format!(
        "📋 [Todos] {project_name} — this session: ⏳{mine_pending} 🔄{mine_in_progress} · recent ({RECENT_WINDOW_DAYS}d): ⏳{recent_pending} 🔄{recent_in_progress} · 🔴{p0_count} 🟠{p1_count}"
    );
    if stale_hidden > 0 {
        use std::fmt::Write as _;
        let _ = write!(context, " · 💤 {stale_hidden} stale hidden");
    }
    if !top_todos.is_empty() {
        context.push_str(" | Top: ");
        context.push_str(&top_todos.join("; "));
    }

    write_session_marker(ctx.fs, session_id);

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

fn write_session_marker(fs: &dyn FileSystemPort, session_id: Option<&str>) {
    if let Some(session_id) = session_id {
        let _ = fs.write(&session_marker(session_id), b"1");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    fn entry(
        content: &str,
        status: &str,
        priority: u8,
        project: &str,
        session_id: Option<&str>,
        updated_at: Option<&str>,
    ) -> TodoEntry {
        TodoEntry {
            content: content.into(),
            status: status.into(),
            priority: Some(priority),
            project: Some(project.into()),
            session_id: session_id.map(String::from),
            updated_at: updated_at.map(String::from),
        }
    }

    #[test]
    fn test_read_todos_parses_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.jsonl");
        let lines = [
            r#"{"content":"Fix bug","status":"pending","priority":0,"project":"/my/project"}"#,
            r#"{"content":"Add tests","status":"in_progress","priority":1,"project":"/my/project","session_id":"s-1","updated_at":"2026-07-06T12:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let todos: Vec<TodoEntry> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "Fix bug");
        assert_eq!(todos[1].session_id.as_deref(), Some("s-1"));
    }

    #[test]
    fn test_filter_project_todos_exact_match() {
        let todos = vec![
            entry("Task A", "pending", 0, "/my/project", None, None),
            entry("Task B", "pending", 1, "/other/repo", None, None),
        ];
        let filtered = filter_project_todos(&todos, "/my/project");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, "Task A");
    }

    #[test]
    fn test_filter_project_todos_basename_match() {
        let todos = vec![entry(
            "Task A",
            "pending",
            0,
            "/Users/operator/Documents/GitHub/myproject",
            None,
            None,
        )];
        let filtered = filter_project_todos(&todos, "/home/dev/myproject");
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn substring_containment_is_not_a_match() {
        // The 4451 bug: a home-dir cwd whose basename is a PREFIX of every
        // project path must match nothing.
        let todos = vec![
            entry(
                "Task A",
                "pending",
                0,
                r"C:\Users\garys\Documents\GitHub\sentinel",
                None,
                None,
            ),
            entry("Task B", "pending", 0, "/tmp/test-project", None, None),
        ];
        let filtered = filter_project_todos(&todos, r"C:\Users\garys");
        assert!(
            filtered.is_empty(),
            "home-dir basename must not substring-match child project paths"
        );
    }

    #[test]
    fn mixed_separators_still_match_exactly() {
        let todos = vec![entry(
            "Task A",
            "pending",
            0,
            r"C:\Users\garys\Documents\GitHub\sentinel",
            None,
            None,
        )];
        let filtered = filter_project_todos(&todos, "C:/Users/garys/Documents/GitHub/sentinel");
        assert_eq!(
            filtered.len(),
            1,
            "separator spelling must not break equality"
        );
    }

    #[test]
    fn summary_splits_session_recent_and_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let now = chrono::Utc::now().to_rfc3339();
        let rows = [
            // This session's row.
            format!(
                r#"{{"content":"Mine","status":"pending","priority":0,"project":"/repo/alpha","session_id":"todo-sess-1","updated_at":"{now}"}}"#
            ),
            // Another session, fresh.
            format!(
                r#"{{"content":"Recent other","status":"in_progress","priority":1,"project":"/repo/alpha","session_id":"todo-sess-2","updated_at":"{now}"}}"#
            ),
            // Another session, ancient — hidden.
            r#"{"content":"Ancient","status":"pending","priority":0,"project":"/repo/alpha","session_id":"todo-sess-3","updated_at":"2026-01-01T00:00:00Z"}"#.to_string(),
            // Different project — excluded entirely.
            format!(
                r#"{{"content":"Elsewhere","status":"pending","priority":0,"project":"/repo/beta","session_id":"todo-sess-4","updated_at":"{now}"}}"#
            ),
        ];
        let todos_path = tmp
            .path()
            .join(".claude")
            .join("todos")
            .join("active.jsonl");
        std::fs::create_dir_all(todos_path.parent().unwrap()).unwrap();
        std::fs::write(&todos_path, rows.join("\n")).unwrap();

        let marker = session_marker("todo-sess-1");
        let _ = std::fs::remove_file(&marker);

        let input = HookInput {
            session_id: Some("todo-sess-1".to_string()),
            cwd: Some("/repo/alpha".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);
        let injected = output
            .hook_specific_output
            .expect("summary must be injected")
            .additional_context
            .expect("context");

        assert!(injected.starts_with("📋 [Todos] alpha"), "{injected}");
        assert!(injected.contains("this session: ⏳1 🔄0"), "{injected}");
        assert!(injected.contains("recent (14d): ⏳0 🔄1"), "{injected}");
        assert!(injected.contains("💤 1 stale hidden"), "{injected}");
        assert!(injected.contains("🔴 Mine"), "{injected}");
        assert!(!injected.contains("Elsewhere"), "{injected}");
        assert!(!injected.contains("Ancient"), "{injected}");

        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn all_stale_rows_stay_silent() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let rows = [
            r#"{"content":"Ancient","status":"pending","priority":0,"project":"/repo/alpha","session_id":"dead-sess","updated_at":"2026-01-01T00:00:00Z"}"#,
        ];
        let todos_path = tmp
            .path()
            .join(".claude")
            .join("todos")
            .join("active.jsonl");
        std::fs::create_dir_all(todos_path.parent().unwrap()).unwrap();
        std::fs::write(&todos_path, rows.join("\n")).unwrap();

        let marker = session_marker("todo-sess-quiet");
        let _ = std::fs::remove_file(&marker);

        let input = HookInput {
            session_id: Some("todo-sess-quiet".to_string()),
            cwd: Some("/repo/alpha".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);
        assert!(
            output.hook_specific_output.is_none(),
            "a project with only stale foreign rows must inject nothing"
        );

        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn test_process_returns_allow() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn missing_session_does_not_write_unknown_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let unknown_marker = session_marker("unknown");
        let _ = std::fs::remove_file(&unknown_marker);

        let input = HookInput::default();
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(!unknown_marker.exists());
    }

    #[test]
    fn synthetic_unknown_session_does_not_write_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let raw_marker = session_marker(" unknown ");
        let trimmed_marker = session_marker("unknown");
        let _ = std::fs::remove_file(&raw_marker);
        let _ = std::fs::remove_file(&trimmed_marker);

        let input = HookInput {
            session_id: Some(" unknown ".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(!raw_marker.exists());
        assert!(!trimmed_marker.exists());
    }

    #[test]
    fn concrete_session_writes_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let marker = session_marker("todo-session-123");
        let _ = std::fs::remove_file(&marker);

        let input = HookInput {
            session_id: Some("todo-session-123".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(marker.exists());
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn test_todo_entry_defaults() {
        let json = "{}";
        let entry: TodoEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.content, "");
        assert_eq!(entry.status, "");
        assert!(entry.priority.is_none());
        assert!(entry.project.is_none());
        assert!(entry.session_id.is_none());
        assert!(entry.updated_at.is_none());
    }
}
