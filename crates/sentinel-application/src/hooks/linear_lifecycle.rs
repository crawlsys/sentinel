//! Linear Issue Lifecycle Monitor
//!
//! PostToolUse hook that detects Linear issue state changes and injects
//! CronCreate instructions for lifecycle monitoring.
//!
//! Detects:
//! - `mcp__linear__update_issue` with state change → lifecycle monitoring
//! - `mcp__linear__create_issue` → remind to track
//! - `mcp__linear__subscribe_to_issue` → confirm tracking

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process a PostToolUse event for Linear tool calls.
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

    // Issue updated — check if state changed to "In Progress"
    if op == "update_issue" {
        if let Some(state_id) = extract_state_change(input) {
            return HookOutput::inject_context(
                HookEvent::PostToolUse,
                format!(
                    "[Linear Lifecycle] Issue state changed (state_id: {state_id}). \
                     If this issue is now In Progress, consider creating a monitoring loop:\n\
                     ```\n\
                     CronCreate(cron: \"47 * * * *\", recurring: true,\n\
                       prompt: \"Check the current Linear issue status. \
                     If it's been In Progress for >24h without commits, remind Gary. \
                     If blocked, identify the blocker. \
                     If done, remind to update Linear status.\")\n\
                     ```"
                ),
            );
        }
    }

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

/// Extract state_id from update_issue tool input if present.
fn extract_state_change(input: &HookInput) -> Option<String> {
    let tool_input = input.tool_input.as_ref()?;
    tool_input
        .get("state_id")
        .and_then(|v| v.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detects_issue_state_change() {
        let input = HookInput {
            tool_name: Some("mcp__linear__update_issue".to_string()),
            tool_input: Some(serde_json::json!({
                "id": "issue-123",
                "state_id": "state-in-progress"
            })),
            ..Default::default()
        };
        let output = process(&input);
        let ctx = output
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.as_deref());
        assert!(ctx.unwrap().contains("Linear Lifecycle"));
        assert!(ctx.unwrap().contains("CronCreate"));
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
        assert!(process(&HookInput::default()).hook_specific_output.is_none());
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
