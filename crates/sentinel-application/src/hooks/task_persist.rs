//! Task Persist Hook — snapshot task list to persistent markdown + JSON
//!
//! Fires on TaskCreated, TaskCompleted, and Stop events.
//! Scans `~/.claude/tasks/` for the active task list directory,
//! reads all `*.json` task files, and writes a standardized snapshot to
//! `~/.claude/persistent-tasks/{project_hash}/tasks.md` + `tasks.json`.
//!
//! The project hash is derived from the working directory (same approach
//! as todo_interceptor), so tasks are scoped per-project.
//!
//! Storage:
//!   ~/.claude/persistent-tasks/{project_hash}/tasks.md    (human-readable)
//!   ~/.claude/persistent-tasks/{project_hash}/tasks.json  (machine-readable for rehydration)
//!   ~/.claude/persistent-tasks/{project_hash}/meta.json   (project name, cwd, last session)

use chrono::Utc;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// A task read from Claude Code's on-disk format
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Task {
    id: String,
    subject: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "activeForm")]
    active_form: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default, rename = "blockedBy")]
    blocked_by: Vec<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// Persistent task snapshot metadata
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PersistMeta {
    project_hash: String,
    cwd: String,
    session_id: String,
    updated_at: String,
    task_count: usize,
    incomplete_count: usize,
}

/// Compute a project hash from the working directory (first 8 hex chars of SHA-256)
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Get the persistent tasks directory for a project
fn persistent_tasks_dir(project_hash: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("persistent-tasks").join(project_hash))
}

/// Find the active task list directory.
///
/// Strategy: look for the session_id as a directory name first (standalone session),
/// then fall back to the most recently modified directory (team or unknown).
fn find_active_task_dir(session_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let tasks_root = home.join(".claude").join("tasks");

    if !tasks_root.is_dir() {
        return None;
    }

    // First: try session_id directly (standalone sessions use session_id as dir name)
    let session_dir = tasks_root.join(session_id);
    if session_dir.is_dir() && has_task_files(&session_dir) {
        return Some(session_dir);
    }

    // Second: find the most recently modified directory with task files
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    if let Ok(entries) = std::fs::read_dir(&tasks_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !has_task_files(&path) {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(modified) = meta.modified() {
                    if best.as_ref().map_or(true, |(_, best_time)| modified > *best_time) {
                        best = Some((path, modified));
                    }
                }
            }
        }
    }

    best.map(|(path, _)| path)
}

/// Check if a directory contains at least one .json task file (not .lock, not .highwatermark)
fn has_task_files(dir: &PathBuf) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| {
                    let name = e.file_name();
                    let name = name.to_string_lossy();
                    name.ends_with(".json") && !name.starts_with('.')
                })
        })
        .unwrap_or(false)
}

/// Read all tasks from a task list directory
fn read_tasks(dir: &PathBuf) -> Vec<Task> {
    let mut tasks = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                if let Ok(task) = serde_json::from_str::<Task>(&content) {
                    tasks.push(task);
                }
            }
        }
    }
    // Sort by numeric ID
    tasks.sort_by(|a, b| {
        let a_num: u32 = a.id.parse().unwrap_or(u32::MAX);
        let b_num: u32 = b.id.parse().unwrap_or(u32::MAX);
        a_num.cmp(&b_num)
    });
    tasks
}

/// Generate standardized markdown from tasks
fn render_tasks_md(tasks: &[Task], cwd: &str, project_hash: &str, session_id: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let incomplete: Vec<&Task> = tasks.iter().filter(|t| t.status != "completed").collect();
    let completed: Vec<&Task> = tasks.iter().filter(|t| t.status == "completed").collect();

    let mut md = format!(
        "---\nproject_hash: {project_hash}\ncwd: {cwd}\nsession: {session_id}\nupdated: {now}\n\
         total: {}\nincomplete: {}\ncompleted: {}\n---\n\n# Tasks\n\n",
        tasks.len(),
        incomplete.len(),
        completed.len()
    );

    if incomplete.is_empty() && completed.is_empty() {
        md.push_str("*No tasks.*\n");
        return md;
    }

    // Incomplete tasks first
    for task in &incomplete {
        let check = match task.status.as_str() {
            "in_progress" => "~",
            _ => " ",
        };
        md.push_str(&format!("## [{check}] {}. {}\n", task.id, task.subject));
        md.push_str(&format!("- **Status:** {}\n", task.status));
        if !task.blocks.is_empty() {
            md.push_str(&format!("- **Blocks:** {}\n", task.blocks.join(", ")));
        }
        if !task.blocked_by.is_empty() {
            md.push_str(&format!(
                "- **Blocked by:** {}\n",
                task.blocked_by.join(", ")
            ));
        }
        if let Some(owner) = &task.owner {
            md.push_str(&format!("- **Owner:** {owner}\n"));
        }
        if !task.description.is_empty() {
            md.push_str(&format!("- **Description:** {}\n", task.description));
        }
        md.push('\n');
    }

    // Completed tasks
    if !completed.is_empty() {
        md.push_str("## Completed\n\n");
        for task in &completed {
            md.push_str(&format!("- [x] **{}. {}**\n", task.id, task.subject));
        }
        md.push('\n');
    }

    md
}

/// Persist tasks to disk (markdown + JSON + meta)
fn write_persistent_tasks(
    tasks: &[Task],
    cwd: &str,
    project_hash: &str,
    session_id: &str,
) -> Result<(), std::io::Error> {
    let dir = match persistent_tasks_dir(project_hash) {
        Some(d) => d,
        None => return Ok(()),
    };
    std::fs::create_dir_all(&dir)?;

    // Write tasks.md (human-readable)
    let md = render_tasks_md(tasks, cwd, project_hash, session_id);
    std::fs::write(dir.join("tasks.md"), &md)?;

    // Write tasks.json (machine-readable for rehydration)
    let json = serde_json::to_string_pretty(tasks).unwrap_or_default();
    std::fs::write(dir.join("tasks.json"), &json)?;

    // Write meta.json
    let incomplete_count = tasks.iter().filter(|t| t.status != "completed").count();
    let meta = PersistMeta {
        project_hash: project_hash.to_string(),
        cwd: cwd.to_string(),
        session_id: session_id.to_string(),
        updated_at: Utc::now().to_rfc3339(),
        task_count: tasks.len(),
        incomplete_count,
    };
    let meta_json = serde_json::to_string_pretty(&meta).unwrap_or_default();
    std::fs::write(dir.join("meta.json"), &meta_json)?;

    tracing::debug!(
        project_hash,
        task_count = tasks.len(),
        incomplete_count,
        "Persisted tasks to disk"
    );

    Ok(())
}

/// Process task persistence on TaskCreated, TaskCompleted, or Stop events.
///
/// Finds the active task directory, reads all task files, and writes
/// a persistent snapshot to `~/.claude/persistent-tasks/{project_hash}/`.
pub fn process(input: &HookInput) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Find the active task directory
    let task_dir = match find_active_task_dir(session_id) {
        Some(dir) => dir,
        None => {
            tracing::debug!("No active task directory found — skipping persist");
            return HookOutput::allow();
        }
    };

    // Read all tasks
    let tasks = read_tasks(&task_dir);
    if tasks.is_empty() {
        return HookOutput::allow();
    }

    // Compute project hash and persist
    let proj_hash = project_hash(cwd);
    if let Err(e) = write_persistent_tasks(&tasks, cwd, &proj_hash, session_id) {
        tracing::warn!(error = %e, "Failed to persist tasks");
    }

    // Never block
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_project_hash_different() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/corvus");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_render_tasks_md_empty() {
        let md = render_tasks_md(&[], ".", "abc123", "session-1");
        assert!(md.contains("*No tasks.*"));
    }

    #[test]
    fn test_render_tasks_md_with_tasks() {
        let tasks = vec![
            Task {
                id: "1".to_string(),
                subject: "Fix auth".to_string(),
                description: "OAuth2 flow".to_string(),
                active_form: None,
                owner: None,
                status: "in_progress".to_string(),
                blocks: vec!["2".to_string()],
                blocked_by: vec![],
                metadata: None,
            },
            Task {
                id: "2".to_string(),
                subject: "Write tests".to_string(),
                description: "Unit tests".to_string(),
                active_form: None,
                owner: None,
                status: "pending".to_string(),
                blocks: vec![],
                blocked_by: vec!["1".to_string()],
                metadata: None,
            },
            Task {
                id: "3".to_string(),
                subject: "Deploy".to_string(),
                description: "Push to prod".to_string(),
                active_form: None,
                owner: None,
                status: "completed".to_string(),
                blocks: vec![],
                blocked_by: vec![],
                metadata: None,
            },
        ];
        let md = render_tasks_md(&tasks, ".", "abc123", "session-1");
        assert!(md.contains("[~] 1. Fix auth"));
        assert!(md.contains("[ ] 2. Write tests"));
        assert!(md.contains("[x] **3. Deploy**"));
        assert!(md.contains("**Blocks:** 2"));
        assert!(md.contains("**Blocked by:** 1"));
        assert!(md.contains("incomplete: 2"));
        assert!(md.contains("completed: 1"));
    }

    #[test]
    fn test_read_tasks_sorted() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().to_path_buf();

        // Write out of order
        std::fs::write(
            dir.join("3.json"),
            r#"{"id":"3","subject":"Third","description":"","status":"pending","blocks":[],"blockedBy":[]}"#,
        ).unwrap();
        std::fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"First","description":"","status":"pending","blocks":[],"blockedBy":[]}"#,
        ).unwrap();
        std::fs::write(
            dir.join("2.json"),
            r#"{"id":"2","subject":"Second","description":"","status":"pending","blocks":[],"blockedBy":[]}"#,
        ).unwrap();

        let tasks = read_tasks(&dir);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "1");
        assert_eq!(tasks[1].id, "2");
        assert_eq!(tasks[2].id, "3");
    }

    #[test]
    fn test_has_task_files() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().to_path_buf();

        // Empty dir
        assert!(!has_task_files(&dir));

        // Only .lock file
        std::fs::write(dir.join(".lock"), "").unwrap();
        assert!(!has_task_files(&dir));

        // Add a task file
        std::fs::write(dir.join("1.json"), "{}").unwrap();
        assert!(has_task_files(&dir));
    }

    #[test]
    fn test_process_no_tasks() {
        let input = HookInput {
            session_id: Some("nonexistent-session".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }
}
