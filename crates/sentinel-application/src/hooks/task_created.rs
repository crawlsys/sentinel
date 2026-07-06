//! `TaskCreated` hook — enrich tasks with metadata on creation
//!
//! When a task is created, extracts structured metadata from the task
//! subject (priority, skill tags) and logs it for telemetry.

use sentinel_domain::events::{HookInput, HookOutput};

/// Extract priority level from subject prefix like `[P0]`, `[P1]`, etc.
fn extract_priority(subject: &str) -> Option<&str> {
    if subject.starts_with("[P0]") {
        Some("P0")
    } else if subject.starts_with("[P1]") {
        Some("P1")
    } else if subject.starts_with("[P2]") {
        Some("P2")
    } else if subject.starts_with("[P3]") {
        Some("P3")
    } else {
        None
    }
}

/// Extract skill tags from `#tag` patterns in the subject.
fn extract_skill_tags(subject: &str) -> Vec<&str> {
    subject
        .split_whitespace()
        .filter(|word| word.starts_with('#') && word.len() > 1)
        .map(|word| &word[1..])
        .collect()
}

/// Process `TaskCreated` event
///
/// Extracts priority and skill tags from the task subject, then logs
/// enriched metadata for telemetry tracking.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let task_id = input
        .extra
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let task_subject = input
        .extra
        .get("task_subject")
        .and_then(|v| v.as_str())
        .unwrap_or("untitled");

    let team_name = input.extra.get("team_name").and_then(|v| v.as_str());

    // Extract structured metadata from the subject
    let priority = extract_priority(task_subject);
    let skill_tags = extract_skill_tags(task_subject);

    // Check for explicit metadata in task data
    let has_metadata = input.extra.get("task_metadata").is_some();
    let has_checklist = input
        .extra
        .get("task_checklist")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());

    tracing::debug!(
        task_id,
        task_subject,
        ?team_name,
        ?priority,
        ?skill_tags,
        has_metadata,
        has_checklist,
        "Task created"
    );

    // Keep the Active Tasks section of ~/.claude/CLAUDE.md in sync with live
    // task state. Fire-and-forget: failure here must never block task
    // creation. Pass the SESSION's cwd (not the hook-process cwd) so the table
    // renders THIS session's project tasks, not another project's.
    let session_cwd = input.cwd.clone().unwrap_or_else(|| ".".to_string());
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::session_init::regenerate_global_claude_md_for(std::path::Path::new(&session_cwd))
    }));

    // Allow task creation — no blocking or modification needed
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_created_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("42"));
        input
            .extra
            .insert("task_subject".to_string(), serde_json::json!("Fix bug"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_extract_priority() {
        assert_eq!(extract_priority("[P0] Fix critical bug"), Some("P0"));
        assert_eq!(extract_priority("[P1] Add feature"), Some("P1"));
        assert_eq!(extract_priority("[P2] Refactor code"), Some("P2"));
        assert_eq!(extract_priority("[P3] Nice to have"), Some("P3"));
        assert_eq!(extract_priority("No priority here"), None);
    }

    #[test]
    fn test_extract_skill_tags() {
        let tags = extract_skill_tags("[P0] Fix SQL injection #bug #security");
        assert_eq!(tags, vec!["bug", "security"]);

        let tags = extract_skill_tags("No tags here");
        assert!(tags.is_empty());

        let tags = extract_skill_tags("[P1] Feature #feature #ddd #test");
        assert_eq!(tags, vec!["feature", "ddd", "test"]);
    }

    #[test]
    fn test_task_created_with_metadata_and_checklist() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("7"));
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("[P1] Implement auth #feature #security"),
        );
        input.extra.insert(
            "task_metadata".to_string(),
            serde_json::json!({"priority": "P1", "phase": "auth"}),
        );
        input.extra.insert(
            "task_checklist".to_string(),
            serde_json::json!([{"id": "1", "text": "Step 1", "completed": false}]),
        );

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
