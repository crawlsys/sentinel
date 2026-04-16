//! TaskCompleted hook — verification gate for task completion
//!
//! When a task is being marked complete, reminds the teammate to verify
//! their work before marking it done. This is the team-level equivalent
//! of the verification_gate hook.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Extract a Linear issue ID from a task subject containing `@linear:{ID}`.
///
/// Returns `Some("PREFIX-123")` if found, `None` otherwise.
fn extract_linear_id(subject: &str) -> Option<&str> {
    let marker = "@linear:";
    let start = subject.find(marker)?;
    let after = &subject[start + marker.len()..];
    // Linear IDs are PREFIX-NUMBER (e.g. FIR-123, SYN-42)
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    let id = &after[..end];
    // Validate shape: at least one letter, a hyphen, at least one digit
    if let Some(hyphen) = id.find('-') {
        let prefix = &id[..hyphen];
        let number = &id[hyphen + 1..];
        if !prefix.is_empty()
            && prefix.chars().all(|c| c.is_ascii_alphanumeric())
            && !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit())
        {
            return Some(id);
        }
    }
    None
}

/// Process TaskCompleted event
///
/// Injects context reminding the teammate to verify before marking complete.
/// If the task subject contains `@linear:{ID}`, also injects Linear sync instructions.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let task_subject = input
        .extra
        .get("task_subject")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown task");

    let teammate_name = input
        .extra
        .get("teammate_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let team_name = input
        .extra
        .get("team_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let task_id = input
        .extra
        .get("task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    // Base verification reminder
    let mut context = format!(
        "[Task Completion Gate] Teammate '{}' (team: {}) is completing task #{}: '{}'\n\
         \n\
         BEFORE marking this task complete, verify:\n\
         1. All acceptance criteria from the task description are met\n\
         2. Tests pass (run them, don't assume)\n\
         3. No TODO/FIXME/HACK markers left in changed code\n\
         4. Changes are committed (or staged for the lead to review)\n\
         5. Report what was done via SendMessage to the team lead",
        teammate_name, team_name, task_id, task_subject
    );

    // Check for incomplete checklist items
    if let Some(checklist) = input.extra.get("task_checklist").and_then(|v| v.as_array()) {
        if !checklist.is_empty() {
            let incomplete: Vec<&str> = checklist
                .iter()
                .filter(|item| !item.get("completed").and_then(|v| v.as_bool()).unwrap_or(false))
                .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                .collect();
            if !incomplete.is_empty() {
                context.push_str(&format!(
                    "\n\n⚠ [Checklist Warning] {} of {} checklist items are NOT completed:\n{}",
                    incomplete.len(),
                    checklist.len(),
                    incomplete
                        .iter()
                        .map(|t| format!("  - [ ] {t}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
    }

    // If task is bound to a Linear issue, append sync instructions
    if let Some(linear_id) = extract_linear_id(task_subject) {
        context.push_str(&format!(
            "\n\n\
             [Linear Sync] Task is bound to Linear issue {}.\n\
             After verifying the task is complete:\n\
             1. Post a progress comment on {} via mcp__linear__create_comment\n\
             2. Check if ALL tasks with @linear:{} are now completed (use TaskList)\n\
             3. If all tasks done → transition the Linear issue to the next workflow state\n\
             4. If tasks remain → note progress in the comment (e.g., \"3/5 tasks complete\")",
            linear_id, linear_id, linear_id
        ));
    }

    // Emit channel event for real-time push notification
    let summary = format!("Task #{task_id} completed: '{task_subject}' (by {teammate_name})");
    let mut meta = serde_json::Map::new();
    meta.insert(
        "task_id".to_string(),
        serde_json::Value::String(task_id.to_string()),
    );
    meta.insert(
        "task_subject".to_string(),
        serde_json::Value::String(task_subject.to_string()),
    );
    meta.insert(
        "teammate_name".to_string(),
        serde_json::Value::String(teammate_name.to_string()),
    );
    crate::channel_events::emit(
        "task_completed", &summary, meta,
        input.session_id.as_deref(), input.cwd.as_deref(), Some("task_completed"),
    );

    HookOutput::inject_context(HookEvent::TaskCompleted, &context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_completed_injects_context() {
        let mut input = HookInput::default();
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("Implement auth"),
        );
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("auth-team"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("42"));

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("backend-dev"));
        assert!(ctx.contains("auth-team"));
        assert!(ctx.contains("Implement auth"));
        assert!(ctx.contains("#42"));
    }

    #[test]
    fn test_task_completed_handles_missing_fields() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("unknown task"));
    }

    #[test]
    fn test_extract_linear_id_valid() {
        assert_eq!(
            extract_linear_id("[P1] Implement auth #feature @linear:FIR-123"),
            Some("FIR-123")
        );
    }

    #[test]
    fn test_extract_linear_id_end_of_string() {
        assert_eq!(extract_linear_id("Task @linear:SYN-42"), Some("SYN-42"));
    }

    #[test]
    fn test_extract_linear_id_missing() {
        assert_eq!(extract_linear_id("[P0] Fix bug #security"), None);
    }

    #[test]
    fn test_task_completed_with_incomplete_checklist() {
        let mut input = HookInput::default();
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("Build feature"),
        );
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("dev-1"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("team-a"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("5"));
        input.extra.insert(
            "task_checklist".to_string(),
            serde_json::json!([
                {"id": "1", "text": "Design API", "completed": true},
                {"id": "2", "text": "Write tests", "completed": false},
                {"id": "3", "text": "Update docs", "completed": false}
            ]),
        );

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("[Checklist Warning]"));
        assert!(ctx.contains("2 of 3"));
        assert!(ctx.contains("Write tests"));
        assert!(ctx.contains("Update docs"));
    }

    #[test]
    fn test_task_completed_with_linear_tag() {
        let mut input = HookInput::default();
        input.extra.insert(
            "task_subject".to_string(),
            serde_json::json!("[P1] Implement auth @linear:FIR-123"),
        );
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("fir-team"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("7"));

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("[Linear Sync]"));
        assert!(ctx.contains("FIR-123"));
        assert!(ctx.contains("mcp__linear__create_comment"));
    }
}
