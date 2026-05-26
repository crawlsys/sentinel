//! `CwdChanged` hook — detect working directory changes
//!
//! When the working directory changes, re-detects project context
//! for skill routing and project config.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process `CwdChanged` event
///
/// Logs directory change for state tracking.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let old_cwd = input
        .extra
        .get("old_cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let new_cwd = input
        .extra
        .get("new_cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(old_cwd, new_cwd, "Working directory changed");

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cwd_changed_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("old_cwd".to_string(), serde_json::json!("/old/path"));
        input
            .extra
            .insert("new_cwd".to_string(), serde_json::json!("/new/path"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
