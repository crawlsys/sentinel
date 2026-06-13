//! Todo Interceptor Hook
//!
//! Intercepts TaskCreate/TaskUpdate tool calls, parses encoded metadata
//! (priority, tags, task IDs), and persists to a rich JSONL format.
//!
//! Runs on `PostToolUse` — only intercepts `TaskCreate` and `TaskUpdate` calls.
//! Never blocks, only persists.
//!
//! Storage:
//!   ~/.claude/todos/active.jsonl
//!   ~/.claude/todos/completed.jsonl
//!   ~/.claude/todos/analytics/quick-stats.json

use std::fmt::Write as _;
use std::path::PathBuf;

use chrono::Utc;
use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};

use super::{FileSystemPort, HookContext};

/// Tool names we intercept
const TASK_CREATE: &str = "TaskCreate";
const TASK_UPDATE: &str = "TaskUpdate";

/// Compute a project hash from the working directory. Delegates to the shared
/// canonical implementation in `super::project_hash` so worktrees of the same
/// repo collapse to the same hash.
fn project_hash(cwd: &str) -> String {
    super::project_hash(cwd)
}

/// Base directory for todos — flat, no project hash subdirectory
fn todos_base_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir().map(|h| h.join(".claude").join("todos"))
}

/// A parsed rich todo item
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RichTodo {
    id: String,
    content: String,
    raw_content: String,
    priority: u8,
    tags: Vec<String>,
    status: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    task_id: String,
    session_id: String,
    project: String,
    project_hash: String,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
}

/// Parse priority from content: [P0], [P1], [P2], [P3]. Default P2.
fn parse_priority(content: &str) -> u8 {
    let re = Regex::new(r"\[P([0-3])\]").unwrap();
    re.captures(content)
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u8>().ok())
        .unwrap_or(2)
}

/// Parse tags from content: #tag1 #tag2
fn parse_tags(content: &str) -> Vec<String> {
    let re = Regex::new(r"#([a-zA-Z0-9_-]+)").unwrap();
    re.captures_iter(content)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

/// Parse task ID from content: AUTH-01:, BUG-42:
fn parse_task_id(content: &str) -> String {
    let re = Regex::new(r"([A-Z]+-[0-9]+):").unwrap();
    re.captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_default()
}

/// Clean description by removing priority, tags, task ID prefix
fn clean_description(content: &str) -> String {
    let cleaned = Regex::new(r"\[P[0-3]\]\s*")
        .unwrap()
        .replace_all(content, "");
    let cleaned = Regex::new(r"#[a-zA-Z0-9_-]+")
        .unwrap()
        .replace_all(&cleaned, "");
    let cleaned = Regex::new(r"^[A-Z]+-[0-9]+:\s*")
        .unwrap()
        .replace(&cleaned, "");
    cleaned.trim().to_string()
}

/// Read existing active todos from a JSONL file
fn read_existing_todos(fs: &dyn FileSystemPort, path: &PathBuf) -> Vec<RichTodo> {
    let content = match fs.read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| serde_json::from_str::<RichTodo>(line).ok())
        .collect()
}

/// Write todos back to a JSONL file (replaces entire file)
fn write_todos(fs: &dyn FileSystemPort, path: &PathBuf, todos: &[RichTodo]) {
    let content: String = todos
        .iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .collect::<Vec<_>>()
        .join("\n");
    if content.is_empty() {
        let _ = fs.write(path, b"");
    } else {
        let _ = fs.write(path, format!("{content}\n").as_bytes());
    }
}

/// Append todos to a JSONL file
fn append_todos(fs: &dyn FileSystemPort, path: &PathBuf, todos: &[RichTodo]) {
    if todos.is_empty() {
        return;
    }
    let content: String = todos
        .iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .fold(String::new(), |mut acc, s| {
            let _ = writeln!(acc, "{s}");
            acc
        });
    let _ = fs.append(path, content.as_bytes());
}

/// Quick stats for analytics
#[derive(Debug, serde::Serialize)]
struct QuickStats {
    active_todos: usize,
    completed_todos: usize,
    p0_active: usize,
    p1_active: usize,
    last_updated: String,
}

/// Update quick-stats.json
fn update_stats(
    fs: &dyn FileSystemPort,
    analytics_dir: &PathBuf,
    active_path: &PathBuf,
    completed_path: &PathBuf,
) {
    let active = read_existing_todos(fs, active_path);
    let completed_count = fs
        .read_to_string(completed_path)
        .map_or(0, |c| c.lines().filter(|l| !l.is_empty()).count());

    let stats = QuickStats {
        active_todos: active.len(),
        completed_todos: completed_count,
        p0_active: active.iter().filter(|t| t.priority == 0).count(),
        p1_active: active.iter().filter(|t| t.priority == 1).count(),
        last_updated: Utc::now().to_rfc3339(),
    };

    let stats_path = analytics_dir.join("quick-stats.json");
    let _ = fs.write(
        &stats_path,
        serde_json::to_string_pretty(&stats)
            .unwrap_or_default()
            .as_bytes(),
    );
}

/// Handle a `TaskCreate` call — add a new task to active.jsonl
fn handle_task_create(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool_input = match &input.tool_input {
        Some(ti) => ti,
        None => return HookOutput::allow(),
    };

    let subject = tool_input
        .get("subject")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let description = tool_input
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");

    if subject.is_empty() {
        return HookOutput::allow();
    }

    // Combine subject + description for metadata parsing
    let raw_content = if description.is_empty() {
        subject.to_string()
    } else {
        format!("{subject}: {description}")
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let timestamp = Utc::now().to_rfc3339();
    let proj_hash = project_hash(cwd);

    let base_dir = match todos_base_dir(ctx.fs) {
        Some(d) => d,
        None => return HookOutput::allow(),
    };
    let analytics_dir = base_dir.join("analytics");
    let _ = ctx.fs.create_dir_all(&analytics_dir);

    let active_path = base_dir.join("active.jsonl");
    let completed_path = base_dir.join("completed.jsonl");

    let priority = parse_priority(&raw_content);
    let tags = parse_tags(&raw_content);
    let task_id = parse_task_id(&raw_content);
    let content = clean_description(subject);

    let rich_todo = RichTodo {
        id: format!("todo_{}_{}", Utc::now().timestamp_millis(), &proj_hash[..4]),
        content,
        raw_content,
        priority,
        tags,
        status: "pending".to_string(),
        task_id,
        session_id: session_id.to_string(),
        project: cwd.to_string(),
        project_hash: proj_hash,
        created_at: timestamp.clone(),
        updated_at: timestamp,
        completed_at: None,
    };

    append_todos(ctx.fs, &active_path, &[rich_todo]);
    update_stats(ctx.fs, &analytics_dir, &active_path, &completed_path);

    HookOutput::allow()
}

/// Handle a `TaskUpdate` call — update status, move to completed if done
fn handle_task_update(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool_input = match &input.tool_input {
        Some(ti) => ti,
        None => return HookOutput::allow(),
    };

    let task_id = tool_input
        .get("taskId")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let new_status = tool_input
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let new_subject = tool_input.get("subject").and_then(|s| s.as_str());

    if task_id.is_empty() {
        return HookOutput::allow();
    }

    let base_dir = match todos_base_dir(ctx.fs) {
        Some(d) => d,
        None => return HookOutput::allow(),
    };
    let analytics_dir = base_dir.join("analytics");
    let _ = ctx.fs.create_dir_all(&analytics_dir);

    let active_path = base_dir.join("active.jsonl");
    let completed_path = base_dir.join("completed.jsonl");

    let mut active_todos = read_existing_todos(ctx.fs, &active_path);
    let timestamp = Utc::now().to_rfc3339();

    // Find by position (task IDs are 1-based indices in Claude Code)
    // Also try matching by content substring as fallback
    let idx = active_todos
        .iter()
        .position(|t| t.id.ends_with(task_id) || t.task_id == task_id);

    if let Some(idx) = idx {
        let mut todo = active_todos.remove(idx);
        todo.updated_at.clone_from(&timestamp);

        if let Some(subj) = new_subject {
            todo.content = clean_description(subj);
            todo.raw_content = subj.to_string();
        }

        if !new_status.is_empty() {
            todo.status = new_status.to_string();
        }

        if new_status == "completed" {
            todo.completed_at = Some(timestamp);
            // Write remaining active, append to completed
            write_todos(ctx.fs, &active_path, &active_todos);
            append_todos(ctx.fs, &completed_path, &[todo]);
        } else if new_status == "deleted" {
            // Just remove from active, don't move to completed
            write_todos(ctx.fs, &active_path, &active_todos);
        } else {
            // Put back in active with updated fields
            active_todos.push(todo);
            write_todos(ctx.fs, &active_path, &active_todos);
        }
    }
    // If not found, this is a task we didn't track (e.g. pre-existing) — no-op

    update_stats(ctx.fs, &analytics_dir, &active_path, &completed_path);

    HookOutput::allow()
}

/// Process a todo interceptor hook event (`PostToolUse`)
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    match input.tool_name.as_deref() {
        Some(TASK_CREATE) => handle_task_create(input, ctx),
        Some(TASK_UPDATE) => handle_task_update(input, ctx),
        _ => HookOutput::allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allows_non_task_tool() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_handles_task_create() {
        let input = HookInput {
            tool_name: Some("TaskCreate".to_string()),
            tool_input: Some(serde_json::json!({
                "subject": "[P1] AUTH-01: Fix #auth login bug",
                "description": "The login flow fails on mobile"
            })),
            cwd: Some("/tmp/test-project".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_handles_task_update() {
        let input = HookInput {
            tool_name: Some("TaskUpdate".to_string()),
            tool_input: Some(serde_json::json!({
                "taskId": "1",
                "status": "completed"
            })),
            cwd: Some("/tmp/test-project".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_empty_subject() {
        let input = HookInput {
            tool_name: Some("TaskCreate".to_string()),
            tool_input: Some(serde_json::json!({
                "subject": "",
                "description": ""
            })),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_parse_priority() {
        assert_eq!(parse_priority("[P0] Critical bug"), 0);
        assert_eq!(parse_priority("[P1] High priority"), 1);
        assert_eq!(parse_priority("[P3] Low priority"), 3);
        assert_eq!(parse_priority("No priority here"), 2); // default
    }

    #[test]
    fn test_parse_tags() {
        let tags = parse_tags("[P1] Fix #auth #security issue");
        assert_eq!(tags, vec!["auth", "security"]);
    }

    #[test]
    fn test_parse_tags_empty() {
        let tags = parse_tags("No tags here");
        assert!(tags.is_empty());
    }

    #[test]
    fn test_parse_task_id() {
        assert_eq!(parse_task_id("AUTH-01: Fix login"), "AUTH-01");
        assert_eq!(parse_task_id("BUG-42: Crash on startup"), "BUG-42");
        assert_eq!(parse_task_id("No task ID"), "");
    }

    #[test]
    fn test_clean_description() {
        assert_eq!(
            clean_description("[P0] AUTH-01: Fix #auth login issue"),
            "Fix  login issue"
        );
        assert_eq!(clean_description("Simple task"), "Simple task");
    }

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_project_hash_different_projects() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/corvus");
        assert_ne!(h1, h2);
    }

    /// A real-FS test adapter for roundtrip tests.
    struct TestFs;
    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> {
            dirs::home_dir()
        }
        fn read_to_string(&self, p: &std::path::Path) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &std::path::Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &std::path::Path) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(&self, _: &std::path::Path) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, p: &std::path::Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &std::path::Path) -> bool {
            p.is_dir()
        }
        fn metadata(&self, p: &std::path::Path) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, p: &std::path::Path, c: &[u8]) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)?;
            Ok(f.write_all(c)?)
        }
    }

    #[test]
    fn test_write_and_read_todos_roundtrip() {
        let fs = TestFs;
        let tmpdir = tempfile::tempdir().unwrap();
        let active_path = tmpdir.path().join("active.jsonl");

        let todo = RichTodo {
            id: "todo_123_abcd".to_string(),
            content: "Fix auth bug".to_string(),
            raw_content: "Fix auth bug".to_string(),
            priority: 1,
            tags: vec!["auth".to_string()],
            status: "pending".to_string(),
            task_id: String::new(),
            session_id: "test".to_string(),
            project: "/tmp/test".to_string(),
            project_hash: "abcd1234".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            completed_at: None,
        };

        // Write
        write_todos(&fs, &active_path, &[todo.clone()]);

        // Read back
        let loaded = read_existing_todos(&fs, &active_path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "Fix auth bug");
        assert_eq!(loaded[0].priority, 1);
        assert_eq!(loaded[0].status, "pending");

        // Append another
        let todo2 = RichTodo {
            id: "todo_456_abcd".to_string(),
            content: "Add tests".to_string(),
            raw_content: "Add tests".to_string(),
            priority: 2,
            tags: vec![],
            status: "pending".to_string(),
            task_id: String::new(),
            session_id: "test".to_string(),
            project: "/tmp/test".to_string(),
            project_hash: "abcd1234".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            completed_at: None,
        };
        append_todos(&fs, &active_path, &[todo2]);

        let loaded = read_existing_todos(&fs, &active_path);
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_no_tool_input() {
        let input = HookInput {
            tool_name: Some("TaskCreate".to_string()),
            tool_input: None,
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
