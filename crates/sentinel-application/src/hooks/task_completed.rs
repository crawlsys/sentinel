//! TaskCompleted hook — verification gate for task completion
//!
//! When a task is being marked complete, reminds the teammate to verify
//! their work before marking it done. This is the team-level equivalent
//! of the verification_gate hook.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process TaskCompleted event
///
/// Injects context reminding the teammate to verify before marking complete.
pub fn process(input: &HookInput) -> HookOutput {
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

    // Inject verification reminder
    let context = format!(
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

    HookOutput::inject_context(HookEvent::TaskCompleted, &context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_completed_injects_context() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("task_subject".to_string(), serde_json::json!("Implement auth"));
        input
            .extra
            .insert("teammate_name".to_string(), serde_json::json!("backend-dev"));
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("auth-team"));
        input
            .extra
            .insert("task_id".to_string(), serde_json::json!("42"));

        let output = process(&input);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("backend-dev"));
        assert!(ctx.contains("auth-team"));
        assert!(ctx.contains("Implement auth"));
        assert!(ctx.contains("#42"));
    }

    #[test]
    fn test_task_completed_handles_missing_fields() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("unknown task"));
    }
}
