//! Task Rehydrate Hook — inject persistent tasks on SessionStart
//!
//! Fires on SessionStart. Reads `~/.claude/sentinel/persistent-tasks/{project_hash}/tasks.json`
//! and injects incomplete tasks into context as a system reminder so Claude
//! sees prior work and can continue where the previous session left off.
//! (Legacy `~/.claude/persistent-tasks/` data is migrated automatically on
//! first read — see `super::migrate_persistent_tasks_dir`.)
//!
//! Only injects tasks that are NOT completed — completed tasks are mentioned
//! as a summary count but not listed in full.

use chrono::Utc;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sha2::{Digest, Sha256};
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

/// Compute project hash (must match task_persist.rs)
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
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

/// Read tasks from the persistent JSON file
fn read_persistent_tasks(fs: &dyn FileSystemPort, project_hash: &str) -> Option<Vec<Task>> {
    let dir = persistent_tasks_dir(fs, project_hash)?;
    let path = dir.join("tasks.json");
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
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

/// Process SessionStart — inject persistent tasks into context
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let proj_hash = project_hash(cwd);

    // Read persistent tasks
    let tasks = match read_persistent_tasks(ctx.fs, &proj_hash) {
        Some(t) if !t.is_empty() => t,
        _ => return HookOutput::allow(),
    };

    // Check if these are from the current session (skip rehydration)
    if let Some(meta) = read_meta(ctx.fs, &proj_hash) {
        if is_current_session(&meta, session_id) {
            tracing::debug!("Persistent tasks are from current session — skipping rehydration");
            return HookOutput::allow();
        }
    }

    // Separate incomplete and completed
    let incomplete: Vec<&Task> = tasks.iter().filter(|t| t.status != "completed").collect();
    let completed_count = tasks.iter().filter(|t| t.status == "completed").count();

    if incomplete.is_empty() {
        return HookOutput::allow();
    }

    // Read meta for timestamp
    let time_str = read_meta(ctx.fs, &proj_hash)
        .map(|m| relative_time(&m.updated_at))
        .unwrap_or_else(|| "unknown".to_string());

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
        context.push_str(&format!(
            "\n#{} [{status_icon}] {} ({})",
            task.id, task.subject, task.status
        ));
        if !task.blocks.is_empty() {
            context.push_str(&format!(" [blocks: {}]", task.blocks.join(", ")));
        }
        if !task.blocked_by.is_empty() {
            context.push_str(&format!(" [blocked by: {}]", task.blocked_by.join(", ")));
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
                    context.push_str(&format!(" [{}]", meta_parts.join(", ")));
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
            context.push_str(&format!("\n  {desc}"));
        }
        // Render checklist progress
        if !task.checklist.is_empty() {
            let done = task.checklist.iter().filter(|c| c.completed).count();
            context.push_str(&format!(
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
            ));
        }
    }

    if completed_count > 0 {
        context.push_str(&format!(
            "\n\n({completed_count} completed task(s) from previous session)"
        ));
    }

    // Always ask before rehydrating — regardless of Autopilot/Planned mode.
    // Gary may have moved on from stale work and doesn't want tasks
    // auto-recreated without consent.
    let ask_instruction = if has_blocking {
        format!(
            "\n\nINSTRUCTION (ASK FIRST): Do NOT auto-recreate these tasks. \
             Ask Gary: \"Found {} incomplete task(s) from a previous session — rehydrate them? (y/n)\". \
             If yes, recreate using TaskCreate + TaskUpdate(addBlockedBy) to wire blocking chains exactly as shown. \
             If no or unclear, skip silently and proceed with the user's opening prompt.",
            incomplete.len()
        )
    } else {
        format!(
            "\n\nINSTRUCTION (ASK FIRST): Do NOT auto-recreate these tasks. \
             Ask Gary: \"Found {} incomplete task(s) from a previous session — rehydrate them? (y/n)\". \
             If yes, recreate using TaskCreate with the exact subjects and descriptions shown above. \
             If no or unclear, skip silently and proceed with the user's opening prompt.",
            incomplete.len()
        )
    };
    context.push_str(&ask_instruction);

    HookOutput::inject_context(HookEvent::SessionStart, &context)
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
}
