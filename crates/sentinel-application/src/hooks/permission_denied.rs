//! PermissionDenied hook — handle auto-mode permission denials
//!
//! Called when auto-mode denies a tool call. Can return `retry: true`
//! via hookSpecificOutput to let the model retry the denied call.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process PermissionDenied event
///
/// Logs the denial. Does not auto-retry — that would bypass the
/// permission system's intent. Just observes for diagnostics.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let reason = input
        .extra
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let tool_name = input.tool_name.as_deref().unwrap_or("unknown");

    tracing::debug!(tool_name, reason, "Permission denied for tool call");

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_denied_allows() {
        let mut input = HookInput::default();
        input.tool_name = Some("Bash".to_string());
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("auto_denied"));

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
