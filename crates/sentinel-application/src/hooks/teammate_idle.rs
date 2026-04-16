//! TeammateIdle hook — quality gate for agent team members going idle
//!
//! When a teammate is about to go idle, checks if they have uncompleted tasks
//! and reminds them to check the task list before going idle.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process TeammateIdle event
///
/// Injects context reminding the teammate to check for remaining work.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
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

    // Inject a reminder to check task list before going idle
    let context = format!(
        "[Team Quality Gate] Teammate '{}' (team: {}) is going idle.\n\
         \n\
         Before going idle, ensure:\n\
         1. All assigned tasks are marked completed (TaskUpdate with status: completed)\n\
         2. Any blockers or issues are reported to the team lead via SendMessage\n\
         3. Check TaskList for any unblocked tasks you can claim\n\
         4. If no more work available, acknowledge to the lead before going idle",
        teammate_name, team_name
    );

    // Emit channel event so the lead session gets a real-time push notification
    let summary = format!("Teammate '{teammate_name}' (team: {team_name}) is going idle.");
    let mut meta = serde_json::Map::new();
    meta.insert(
        "teammate_name".to_string(),
        serde_json::Value::String(teammate_name.to_string()),
    );
    meta.insert(
        "team_name".to_string(),
        serde_json::Value::String(team_name.to_string()),
    );
    crate::channel_events::emit(
        "teammate_idle", &summary, meta,
        input.session_id.as_deref(), input.cwd.as_deref(), Some(teammate_name),
    );

    HookOutput::inject_context(HookEvent::TeammateIdle, &context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_teammate_idle_injects_context() {
        let mut input = HookInput::default();
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("my-project"));

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("backend-dev"));
        assert!(ctx.contains("my-project"));
        assert!(ctx.contains("TaskList"));
    }

    #[test]
    fn test_teammate_idle_handles_missing_fields() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("unknown"));
    }
}
