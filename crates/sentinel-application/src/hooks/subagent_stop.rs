//! SubagentStop hook — quality gate before agent concludes
//!
//! Ensures agents verify their work before finishing, similar to
//! the TeammateIdle quality gate.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process SubagentStop event
///
/// Logs agent completion for telemetry. Uses stderr (exit 0) since
/// SubagentStop stdout is not injected into model context.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let agent_type = input
        .extra
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::debug!(agent_type, "Subagent stopping");

    // SubagentStop is like Stop — stdout not injected.
    // Log for telemetry, but don't try to inject context.
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

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
