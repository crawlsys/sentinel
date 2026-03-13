//! Todo Interceptor Hook
//!
//! Intercepts TodoWrite tool calls, parses encoded metadata (priority,
//! tags, task IDs), and persists to a rich JSONL format.
//!
//! Runs on PostToolUse — only intercepts TodoWrite calls.
//! Never blocks, only persists.
//!
//! Storage:
//!   ~/.claude/todos/{project_hash}/active.jsonl
//!   ~/.claude/todos/{project_hash}/completed.jsonl
//!   ~/.claude/todos/{project_hash}/analytics/quick-stats.json

use chrono::Utc;
use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Compute a project hash from the working directory (first 8 hex chars of SHA-256)
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    hex_encode(&result[..4])
}

/// Encode bytes as lowercase hex string
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Base directory for todos
fn todos_base_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("todos"))
}

/// A parsed rich todo item
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RichTodo {
    id: String,
    content: String,
    raw_content: String,
    priority: u8,
    tags: Vec<String>,
    status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
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
fn read_existing_todos(path: &PathBuf) -> Vec<RichTodo> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    content
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(|line| serde_json::from_str::<RichTodo>(line).ok())
        .collect()
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

/// Process a todo interceptor hook event (PostToolUse)
pub fn process(input: &HookInput) -> HookOutput {
    // Only intercept TodoWrite calls
    let tool = match &input.tool_name {
        Some(name) if name == "TodoWrite" => name.as_str(),
        _ => return HookOutput::allow(),
    };
    let _ = tool;

    // Extract todos array from tool_input
    let todos_array = match input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("todos"))
        .and_then(|t| t.as_array())
    {
        Some(arr) => arr.clone(),
        None => return HookOutput::allow(),
    };

    if todos_array.is_empty() {
        return HookOutput::allow();
    }

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let timestamp = Utc::now().to_rfc3339();
    let proj_hash = project_hash(cwd);

    // Determine storage directory
    let base_dir = match todos_base_dir() {
        Some(d) => d.join(&proj_hash),
        None => return HookOutput::allow(),
    };
    let analytics_dir = base_dir.join("analytics");

    // Ensure directories exist
    let _ = std::fs::create_dir_all(&analytics_dir);

    let active_path = base_dir.join("active.jsonl");
    let completed_path = base_dir.join("completed.jsonl");

    // Read existing active todos for ID matching
    let existing = read_existing_todos(&active_path);

    let mut new_active: Vec<RichTodo> = Vec::new();
    let mut new_completed: Vec<RichTodo> = Vec::new();

    for todo_val in &todos_array {
        let raw_content = todo_val
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let status = todo_val
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("pending")
            .to_string();

        let priority = parse_priority(&raw_content);
        let tags = parse_tags(&raw_content);
        let task_id = parse_task_id(&raw_content);
        let content = clean_description(&raw_content);

        // Match against existing by content to preserve IDs
        let existing_match = existing.iter().find(|t| t.content == content);
        let id = existing_match.map(|t| t.id.clone()).unwrap_or_else(|| {
            format!("todo_{}_{}", Utc::now().timestamp_millis(), &proj_hash[..4])
        });
        let created_at = existing_match
            .map(|t| t.created_at.clone())
            .unwrap_or_else(|| timestamp.clone());

        let rich_todo = RichTodo {
            id,
            content,
            raw_content,
            priority,
            tags,
            status: status.clone(),
            task_id,
            session_id: session_id.to_string(),
            project: cwd.to_string(),
            project_hash: proj_hash.clone(),
            created_at,
            updated_at: timestamp.clone(),
            completed_at: if status == "completed" {
                Some(timestamp.clone())
            } else {
                None
            },
        };

        if status == "completed" {
            new_completed.push(rich_todo);
        } else {
            new_active.push(rich_todo);
        }
    }

    // Write active todos (replace file)
    let active_content: String = new_active
        .iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .collect::<Vec<_>>()
        .join("\n");
    if !active_content.is_empty() {
        let _ = std::fs::write(&active_path, format!("{active_content}\n"));
    } else {
        let _ = std::fs::write(&active_path, "");
    }

    // Append completed todos
    if !new_completed.is_empty() {
        let completed_content: String = new_completed
            .iter()
            .filter_map(|t| serde_json::to_string(t).ok())
            .map(|s| format!("{s}\n"))
            .collect();
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&completed_path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(completed_content.as_bytes())
            });
    }

    // Update quick stats
    let completed_count = std::fs::read_to_string(&completed_path)
        .map(|c| c.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);

    let stats = QuickStats {
        active_todos: new_active.len(),
        completed_todos: completed_count,
        p0_active: new_active.iter().filter(|t| t.priority == 0).count(),
        p1_active: new_active.iter().filter(|t| t.priority == 1).count(),
        last_updated: timestamp,
    };

    let stats_path = analytics_dir.join("quick-stats.json");
    let _ = std::fs::write(
        &stats_path,
        serde_json::to_string_pretty(&stats).unwrap_or_default(),
    );

    // Never block
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allows_non_todo_tool() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_empty_todos() {
        let input = HookInput {
            tool_name: Some("TodoWrite".to_string()),
            tool_input: Some(serde_json::json!({"todos": []})),
            ..Default::default()
        };
        let output = process(&input);
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

    #[test]
    fn test_processes_todos_to_disk() {
        let tmpdir = tempfile::tempdir().unwrap();
        let cwd = tmpdir.path().to_string_lossy().to_string();
        let proj_hash = project_hash(&cwd);

        // Create the storage dir manually since we're testing
        let base_dir = todos_base_dir().unwrap().join(&proj_hash);
        let analytics_dir = base_dir.join("analytics");
        std::fs::create_dir_all(&analytics_dir).unwrap();

        let input = HookInput {
            tool_name: Some("TodoWrite".to_string()),
            tool_input: Some(serde_json::json!({
                "todos": [
                    {"content": "[P1] AUTH-01: Fix #auth login bug", "status": "pending"},
                    {"content": "Simple task", "status": "completed"}
                ]
            })),
            cwd: Some(cwd.clone()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };

        let output = process(&input);
        assert!(output.blocked.is_none());

        // Verify active.jsonl was written
        let active_path = base_dir.join("active.jsonl");
        let active_content = std::fs::read_to_string(&active_path).unwrap();
        assert!(active_content.contains("Fix"));
        assert!(active_content.contains("auth"));

        // Verify completed.jsonl was written
        let completed_path = base_dir.join("completed.jsonl");
        let completed_content = std::fs::read_to_string(&completed_path).unwrap();
        assert!(completed_content.contains("Simple task"));

        // Verify quick-stats.json
        let stats_path = analytics_dir.join("quick-stats.json");
        let stats_content = std::fs::read_to_string(&stats_path).unwrap();
        assert!(stats_content.contains("\"active_todos\": 1"));
        assert!(stats_content.contains("\"p1_active\": 1"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_no_todos_field() {
        let input = HookInput {
            tool_name: Some("TodoWrite".to_string()),
            tool_input: Some(serde_json::json!({"other_field": "value"})),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }
}
