//! PermissionDenied hook — handle auto-mode permission denials
//!
//! Called when auto-mode denies a tool call. Can return `retry: true`
//! via hookSpecificOutput to let the model retry the denied call.
//!
//! Also forwards the denial to the local sentinel daemon's
//! legatus (if hosted) as a `SessionBlocked{PermissionDenied}`
//! escalation — the operator gets a chat ping naming the denied
//! tool, routed via consul to whichever surface they were last
//! seen on.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_legatus::{BlockReason, EscalationKind};

use crate::legatus_client::escalate_fire_and_forget;

/// Process PermissionDenied event
///
/// Logs the denial. Does not auto-retry — that would bypass the
/// permission system's intent. Just observes for diagnostics and
/// fires a `SessionBlocked` escalation to the daemon-hosted
/// legatus (fire-and-forget; no-op when the daemon isn't
/// running).
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
    let reason = input
        .extra
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let tool_name = input.tool_name.as_deref().unwrap_or("unknown");

    tracing::debug!(tool_name, reason, "Permission denied for tool call");

    escalate_fire_and_forget(EscalationKind::Blocked {
        reason: BlockReason::PermissionDenied {
            tool: tool_name.to_owned(),
        },
    });

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

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
