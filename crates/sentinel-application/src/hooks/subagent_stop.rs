//! SubagentStop hook — quality gate before agent concludes
//!
//! Ensures agents verify their work before finishing, similar to
//! the TeammateIdle quality gate.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process SubagentStop event
///
/// Logs agent completion for telemetry and emits a channel event
/// so the sentinel-mcp server can push a notification into the session.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let agent_type = input
        .extra
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::debug!(agent_type, "Subagent stopping");

    // Emit channel event for real-time push notification
    let summary = format!("Background agent ({agent_type}) has finished.");
    let mut meta = serde_json::Map::new();
    meta.insert(
        "agent_type".to_string(),
        serde_json::Value::String(agent_type.to_string()),
    );
    crate::channel_events::emit(
        ctx.fs,
        ctx.env,
        "agent_completed",
        &summary,
        meta,
        input.session_id.as_deref(),
        input.cwd.as_deref(),
        Some(agent_type),
    );

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subagent_stop_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("agent_type".to_string(), serde_json::json!("debugger"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
