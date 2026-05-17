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

use crate::legatus_client::{escalate_fire_and_forget, note_turn_signal, TurnSignal};

/// Process PermissionDenied event
///
/// Logs the denial. Does not auto-retry — that would bypass the
/// permission system's intent. Observes for diagnostics, fires a
/// `SessionBlocked` escalation to the daemon-hosted legatus
/// (fire-and-forget; no-op when the daemon isn't running), and
/// records a per-session `TurnSignal::PermissionDenied` so the
/// Stop hook can classify pending operator-relayed instructions
/// as `Declined` instead of `Success` for this turn.
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

    if let Some(session_id) = input.session_id.as_deref() {
        note_turn_signal(
            session_id,
            &TurnSignal::PermissionDenied {
                tool: tool_name.to_owned(),
            },
        );
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legatus_client::{take_turn_signals, TurnSignal};

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

    /// PermissionDenied records a `TurnSignal::PermissionDenied`
    /// for the active session so the Stop hook can classify
    /// pending instructions as `Declined { ..tool.. }` instead of
    /// the default `Success`. Uses a fresh per-test session id so
    /// the take-and-clear file ops don't collide with other tests
    /// (or a live sentinel daemon on the same dev host).
    #[test]
    fn test_permission_denied_records_turn_signal() {
        let session_id = format!("perm-denied-test-{}", uuid::Uuid::new_v4());
        let mut input = HookInput::default();
        input.tool_name = Some("Bash".to_string());
        input.session_id = Some(session_id.clone());
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("auto_denied"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let _ = process(&input, &ctx);

        let signals = take_turn_signals(&session_id);
        assert_eq!(signals.len(), 1, "expected one PermissionDenied signal");
        match &signals[0] {
            TurnSignal::PermissionDenied { tool } => assert_eq!(tool, "Bash"),
        }
    }

    /// PermissionDenied without a `session_id` is a no-op for the
    /// turn-signal file — there's no session to scope it to.
    /// Other behavior (escalation fire-and-forget, return value)
    /// is unchanged.
    #[test]
    fn test_permission_denied_without_session_id_skips_signal_record() {
        let mut input = HookInput::default();
        input.tool_name = Some("Edit".to_string());
        // session_id intentionally absent.
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("auto_denied"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
        // No way to assert "no file written" without a session id —
        // the function silently skips. This test just pins that no
        // panic / no return-value regression happens.
    }
}
