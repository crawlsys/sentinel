//! Linear Issue Lifecycle Monitor
//!
//! `PostToolUse` hook that detects Linear issue state changes and injects
//! `CronCreate` instructions for lifecycle monitoring.
//!
//! Detects:
//! - `mcp__linear__update_issue` with state change → lifecycle monitoring
//! - `mcp__linear__create_issue` → remind to track
//! - `mcp__linear__subscribe_to_issue` → confirm tracking

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process a `PostToolUse` event for Linear tool calls.
pub fn process(input: &HookInput) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return HookOutput::allow(),
    };

    // Only react to Linear MCP tools
    if !tool.starts_with("mcp__linear__") {
        return HookOutput::allow();
    }

    let op = tool.strip_prefix("mcp__linear__").unwrap_or("");

    // NOTE: the `update_issue` + `state_id` → CronCreate lifecycle monitor was
    // migrated to the declarative `autocron` hook (rule `linear_state_change`),
    // so cron emission lives in one data-driven place. This hook keeps only the
    // `create_issue` subscribe nudge (advisory prose, not a cron).

    // Issue created — remind to subscribe
    if op == "create_issue" {
        return HookOutput::inject_context(
            HookEvent::PostToolUse,
            "[Linear Lifecycle] Issue created. Consider subscribing to it with \
             `mcp__linear__subscribe_to_issue` to get notifications."
                .to_string(),
        );
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_change_cron_migrated_to_autocron() {
        // The state_id → CronCreate monitor moved to autocron::linear_state_change;
        // this hook no longer emits for update_issue (tested in autocron).
        let input = HookInput {
            tool_name: Some("mcp__linear__update_issue".to_string()),
            tool_input: Some(serde_json::json!({
                "id": "issue-123",
                "state_id": "state-in-progress"
            })),
            ..Default::default()
        };
        assert!(process(&input).hook_specific_output.is_none());
    }

    #[test]
    fn test_detects_issue_creation() {
        let input = HookInput {
            tool_name: Some("mcp__linear__create_issue".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("subscribe"));
    }

    #[test]
    fn test_ignores_linear_read_ops() {
        let input = HookInput {
            tool_name: Some("mcp__linear__get_issue".to_string()),
            ..Default::default()
        };
        assert!(process(&input).hook_specific_output.is_none());
    }

    #[test]
    fn test_ignores_non_linear_tools() {
        let input = HookInput {
            tool_name: Some("mcp__doppler__get_secret".to_string()),
            ..Default::default()
        };
        assert!(process(&input).hook_specific_output.is_none());
    }

    #[test]
    fn test_ignores_no_tool_name() {
        assert!(process(&HookInput::default())
            .hook_specific_output
            .is_none());
    }

    #[test]
    fn test_update_without_state_change() {
        let input = HookInput {
            tool_name: Some("mcp__linear__update_issue".to_string()),
            tool_input: Some(serde_json::json!({
                "id": "issue-123",
                "title": "New title"
            })),
            ..Default::default()
        };
        // No state_id → no lifecycle monitor
        assert!(process(&input).hook_specific_output.is_none());
    }
}
