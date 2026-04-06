//! Setup hook — repo initialization and maintenance
//!
//! Called for repo init/maintenance events. Trigger field indicates
//! whether this is "init" (first setup) or "maintenance" (periodic).

use sentinel_domain::events::{HookInput, HookOutput};

/// Process Setup event
///
/// Logs setup events. Could trigger `sentinel init` for init triggers.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let trigger = input
        .extra
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(trigger, "Setup event received");

    // For now, just log — sentinel init is already handled by SessionStart
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("trigger".to_string(), serde_json::json!("init"));

        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
