//! TaskCreated hook — enrich tasks with metadata on creation
//!
//! When a task is created, logs it for telemetry and could inject
//! additional metadata (skill name, project context).

use sentinel_domain::events::{HookInput, HookOutput};

/// Process TaskCreated event
///
/// Logs task creation for telemetry tracking.
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

    let team_name = input
        .extra
        .get("team_name")
        .and_then(|v| v.as_str());

    tracing::debug!(task_id, task_subject, ?team_name, "Task created");

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

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
