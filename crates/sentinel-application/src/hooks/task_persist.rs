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

use super::{FileSystemPort, HookContext};

/// A single checklist item within a task
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ChecklistItem {
    id: String,
    text: String,
    #[serde(default)]
    completed: bool,
}

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
    checklist: Vec<ChecklistItem>,
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
fn persistent_tasks_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    fs.home_dir().map(|h| h.join(".claude").join("persistent-tasks").join(project_hash))
}

/// Find the active task list directory for this session.
///
/// Strictly scoped to `~/.claude/tasks/{session_id}/`. Returns `None` if that
/// directory doesn't exist or has no task files — callers must treat `None`
/// as "nothing to persist".
///
/// No fallback: scanning `~/.claude/tasks/` for the most recently modified
/// dir leaks tasks across projects. A session in project A would inherit
/// project B's tasks if A's session dir hadn't been created yet.
fn find_active_task_dir(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let session_dir = home.join(".claude").join("tasks").join(session_id);
    if fs.is_dir(&session_dir) && has_task_files(fs, &session_dir) {
        Some(session_dir)
    } else {
        None
    }
}

/// Check if a directory contains at least one .json task file (not .lock, not .highwatermark)
fn has_task_files(fs: &dyn FileSystemPort, dir: &PathBuf) -> bool {
    fs.read_dir(dir)
        .map(|entries| {
            entries.iter().any(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                name.ends_with(".json") && !name.starts_with('.')
            })
        })
        .unwrap_or(false)
}

/// Read all tasks from a task list directory
fn read_tasks(fs: &dyn FileSystemPort, dir: &PathBuf) -> Vec<Task> {
    let mut tasks = Vec::new();
    if let Ok(entries) = fs.read_dir(dir) {
        for path in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            if let Ok(content) = fs.read_to_string(&path) {
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
        // Render structured metadata fields
        if let Some(meta) = &task.metadata {
            if let Some(obj) = meta.as_object() {
                if let Some(priority) = obj.get("priority").and_then(|v| v.as_str()) {
                    md.push_str(&format!("- **Priority:** {priority}\n"));
                }
                if let Some(phase) = obj.get("phase").and_then(|v| v.as_str()) {
                    md.push_str(&format!("- **Phase:** {phase}\n"));
                }
                if let Some(tags) = obj.get("skill_tags").and_then(|v| v.as_array()) {
                    let tag_strs: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
                    if !tag_strs.is_empty() {
                        md.push_str(&format!("- **Tags:** {}\n", tag_strs.join(", ")));
                    }
                }
            }
        }
        if !task.description.is_empty() {
            md.push_str(&format!("- **Description:** {}\n", task.description));
        }
        // Render checklist items
        if !task.checklist.is_empty() {
            let done = task.checklist.iter().filter(|c| c.completed).count();
            md.push_str(&format!(
                "- **Checklist:** ({}/{})\n",
                done,
                task.checklist.len()
            ));
            for item in &task.checklist {
                let mark = if item.completed { "x" } else { " " };
                md.push_str(&format!("  - [{mark}] {}\n", item.text));
            }
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
    fs: &dyn FileSystemPort,
    tasks: &[Task],
    cwd: &str,
    project_hash: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    let dir = match persistent_tasks_dir(fs, project_hash) {
        Some(d) => d,
        None => return Ok(()),
    };
    fs.create_dir_all(&dir)?;

    // Write tasks.md (human-readable)
    let md = render_tasks_md(tasks, cwd, project_hash, session_id);
    fs.write(&dir.join("tasks.md"), md.as_bytes())?;

    // Write tasks.json (machine-readable for rehydration)
    let json = serde_json::to_string_pretty(tasks).unwrap_or_default();
    fs.write(&dir.join("tasks.json"), json.as_bytes())?;

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
    fs.write(&dir.join("meta.json"), meta_json.as_bytes())?;

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
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Find the active task directory
    let task_dir = match find_active_task_dir(ctx.fs, session_id) {
        Some(dir) => dir,
        None => {
            tracing::debug!("No active task directory found — skipping persist");
            return HookOutput::allow();
        }
    };

    // Read all tasks
    let tasks = read_tasks(ctx.fs, &task_dir);
    if tasks.is_empty() {
        return HookOutput::allow();
    }

    // Compute project hash and persist
    let proj_hash = project_hash(cwd);
    if let Err(e) = write_persistent_tasks(ctx.fs, &tasks, cwd, &proj_hash, session_id) {
        tracing::warn!(error = %e, "Failed to persist tasks");
    }

    // Never block
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Minimal real-FS for tests that need to read temp directories.
    struct TestFs;
    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> { dirs::home_dir() }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            if let Some(par) = p.parent() { std::fs::create_dir_all(par)?; }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> { Ok(std::fs::create_dir_all(p)?) }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool { p.exists() }
        fn is_dir(&self, p: &Path) -> bool { p.is_dir() }
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

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
                checklist: vec![
                    ChecklistItem {
                        id: "1".to_string(),
                        text: "Design API".to_string(),
                        completed: true,
                    },
                    ChecklistItem {
                        id: "2".to_string(),
                        text: "Write tests".to_string(),
                        completed: false,
                    },
                ],
                metadata: Some(serde_json::json!({
                    "priority": "P0",
                    "phase": "auth-refactor",
                    "skill_tags": ["feature", "security"]
                })),
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
                checklist: vec![],
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
                checklist: vec![],
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
        // Checklist rendering
        assert!(md.contains("**Checklist:** (1/2)"));
        assert!(md.contains("[x] Design API"));
        assert!(md.contains("[ ] Write tests"));
        // Metadata rendering
        assert!(md.contains("**Priority:** P0"));
        assert!(md.contains("**Phase:** auth-refactor"));
        assert!(md.contains("**Tags:** feature, security"));
    }

    #[test]
    fn test_read_tasks_sorted() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().to_path_buf();

        // Write out of order
        std::fs::write(
            dir.join("3.json"),
            r#"{"id":"3","subject":"Third","description":"","status":"pending","blocks":[],"blockedBy":[],"checklist":[]}"#,
        ).unwrap();
        std::fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"First","description":"","status":"pending","blocks":[],"blockedBy":[],"checklist":[]}"#,
        ).unwrap();
        std::fs::write(
            dir.join("2.json"),
            r#"{"id":"2","subject":"Second","description":"","status":"pending","blocks":[],"blockedBy":[],"checklist":[]}"#,
        ).unwrap();

        let fs = TestFs;
        let tasks = read_tasks(&fs, &dir);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "1");
        assert_eq!(tasks[1].id, "2");
        assert_eq!(tasks[2].id, "3");
    }

    #[test]
    fn test_has_task_files() {
        let fs = TestFs;
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().to_path_buf();

        // Empty dir
        assert!(!has_task_files(&fs, &dir));

        // Only .lock file
        std::fs::write(dir.join(".lock"), "").unwrap();
        assert!(!has_task_files(&fs, &dir));

        // Add a task file
        std::fs::write(dir.join("1.json"), "{}").unwrap();
        assert!(has_task_files(&fs, &dir));
    }

    #[test]
    fn test_process_no_tasks() {
        let input = HookInput {
            session_id: Some("nonexistent-session".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    /// FS that reports a caller-supplied home dir so tests can isolate `~/.claude/`.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> { Some(self.home.clone()) }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> { Ok(std::fs::read_to_string(p)?) }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            if let Some(par) = p.parent() { std::fs::create_dir_all(par)?; }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> { Ok(std::fs::create_dir_all(p)?) }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(std::fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool { p.exists() }
        fn is_dir(&self, p: &Path) -> bool { p.is_dir() }
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> { Ok(std::fs::metadata(p)?) }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

    /// Regression: `find_active_task_dir` must NOT fall back to the most
    /// recently modified dir in `~/.claude/tasks/`. Doing so leaks tasks
    /// across projects (see bug where a session in `C:\Users\garys` picked
    /// up legatus-utility-rust tasks because legatus's dir had newer mtime).
    #[test]
    fn test_find_active_task_dir_no_cross_project_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let tasks_root = home.join(".claude").join("tasks");

        let target_session = "target-session-uuid";
        let other_session = "other-session-uuid";

        // Target session dir with a task file — older mtime.
        let target_dir = tasks_root.join(target_session);
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("1.json"), "{}").unwrap();

        // Other session dir with a task file — guaranteed newer mtime.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let other_dir = tasks_root.join(other_session);
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(other_dir.join("1.json"), "{}").unwrap();

        let fs = ScopedHomeFs { home };

        // Lookup by matching session_id returns its own dir, not the newer one.
        let found = find_active_task_dir(&fs, target_session).unwrap();
        assert_eq!(found, target_dir);

        // Lookup for a session with no dir returns None — not the newest sibling.
        let missing = find_active_task_dir(&fs, "no-such-session");
        assert!(missing.is_none(), "must not fall back to other sessions' dirs");
    }

    #[test]
    fn test_find_active_task_dir_missing_tasks_root() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs { home: tmp.path().to_path_buf() };
        // ~/.claude/tasks/ doesn't exist at all.
        assert!(find_active_task_dir(&fs, "any-session").is_none());
    }

    #[test]
    fn test_find_active_task_dir_empty_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let session_dir = home.join(".claude").join("tasks").join("session-x");
        std::fs::create_dir_all(&session_dir).unwrap();
        // Dir exists but has no .json task files.
        let fs = ScopedHomeFs { home };
        assert!(find_active_task_dir(&fs, "session-x").is_none());
    }
}
