//! Todo Loader Hook
//!
//! Runs on `UserPromptSubmit`. Reads persistent todos from
//! `~/.claude/todos/active.jsonl`, filters by current project,
//! groups by status/priority, and injects a summary into context.
//! Only loads once per session (uses a temp marker file).
//!
//! All IO goes through `ctx.fs` (`FileSystemPort`).

use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{concrete_input_session_id, FileSystemPort, HookContext};

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

/// Filter todos relevant to the current project
fn filter_project_todos<'a>(todos: &'a [TodoEntry], cwd: &str) -> Vec<&'a TodoEntry> {
    let project_name = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");

    todos
        .iter()
        .filter(|t| {
            if let Some(ref proj) = t.project {
                proj == cwd || proj.contains(project_name)
            } else {
                false
            }
        })
        .collect()
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

    let project_todos = filter_project_todos(&all_todos, cwd);

    if project_todos.is_empty() {
        write_session_marker(ctx.fs, session_id);
        return HookOutput::allow();
    }

    // Count by status
    let pending_count = project_todos
        .iter()
        .filter(|t| t.status == "pending")
        .count();
    let in_progress_count = project_todos
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();

    if pending_count == 0 && in_progress_count == 0 {
        write_session_marker(ctx.fs, session_id);
        return HookOutput::allow();
    }

    // Count by priority (active only)
    let active: Vec<&&TodoEntry> = project_todos
        .iter()
        .filter(|t| t.status == "pending" || t.status == "in_progress")
        .collect();

    let p0_count = active.iter().filter(|t| t.priority == Some(0)).count();
    let p1_count = active.iter().filter(|t| t.priority == Some(1)).count();

    // Top 5 todo summaries sorted by priority
    let mut sorted_active: Vec<&TodoEntry> = active.iter().copied().copied().collect();
    sorted_active.sort_by_key(|t| t.priority.unwrap_or(2));
    let top_todos: Vec<&str> = sorted_active
        .iter()
        .take(5)
        .map(|t| t.content.as_str())
        .collect();

    let project_name = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let mut context = format!(
        "[Todos] Project: {project_name} | Pending: {pending_count} | In Progress: {in_progress_count} | P0: {p0_count} | P1: {p1_count}"
    );

    if !top_todos.is_empty() {
        context.push_str(" | Top items: ");
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

    fn make_todo(content: &str, status: &str, priority: u8, project: &str) -> String {
        format!(
            r#"{{"content":"{}","status":"{}","priority":{},"project":"{}"}}"#,
            content, status, priority, project
        )
    }

    #[test]
    fn test_read_todos_parses_entries() {
        // Use a real tempfile since read_todos takes a Path
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("active.jsonl");
        let lines = vec![
            make_todo("Fix bug", "pending", 0, "/my/project"),
            make_todo("Add tests", "in_progress", 1, "/my/project"),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let fs = crate::hooks::test_support::StubFs;
        // StubFs returns "not found" — use RealFs behavior via direct read_todos with a real path
        // For this test, we test the parsing logic directly
        let content = std::fs::read_to_string(&path).unwrap();
        let todos: Vec<TodoEntry> = content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "Fix bug");
        let _ = fs; // suppress unused
    }

    #[test]
    fn test_filter_project_todos_exact_match() {
        let todos = vec![
            TodoEntry {
                content: "Task A".into(),
                status: "pending".into(),
                priority: Some(0),
                project: Some("/my/project".into()),
            },
            TodoEntry {
                content: "Task B".into(),
                status: "pending".into(),
                priority: Some(1),
                project: Some("/other/repo".into()),
            },
        ];
        let filtered = filter_project_todos(&todos, "/my/project");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, "Task A");
    }

    #[test]
    fn test_filter_project_todos_basename_match() {
        let todos = vec![TodoEntry {
            content: "Task A".into(),
            status: "pending".into(),
            priority: Some(0),
            project: Some("/Users/gary/Documents/GitHub/myproject".into()),
        }];
        let filtered = filter_project_todos(&todos, "/home/dev/myproject");
        assert_eq!(filtered.len(), 1);
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
    }
}
