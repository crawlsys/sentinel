//! `SubagentStop` hook — quality gate before agent concludes
//!
//! Ensures agents verify their work before finishing, similar to
//! the `TeammateIdle` quality gate.
//!
//! ## Layered (redundant) false-done enforcement
//!
//! Praetorian principle: "any single enforcement mechanism can fail."
//! The claim-reality-check sweep runs on the main thread's `Stop`, but a
//! background agent that marks a task ✅ and then concludes never triggers
//! `Stop` — so its false-done claim could slip through until the parent
//! happens to Stop. We therefore run the SAME reality-check sweep here, so
//! both terminal events enforce it. The check is itself fail-open and
//! throttled per-session (it won't double-flag a task the Stop sweep
//! already saw), so running it in both places is safe and idempotent.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process `SubagentStop` event
///
/// Logs agent completion for telemetry, emits a channel event so the
/// sentinel-mcp server can push a notification into the session, AND runs
/// the shared claim-reality-check sweep (layered enforcement — see module
/// docs).
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

    // Layered enforcement: run the same false-done sweep the Stop arm runs.
    // Fail-open + per-session throttled inside, so this never blocks the
    // subagent and never double-flags.
    let mut out = HookOutput::allow();
    out.merge(&super::claim_reality_check::process(input, ctx));
    out
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

    #[test]
    fn test_subagent_stop_runs_reality_check_fail_open() {
        // Layered enforcement: the shared reality-check sweep runs here too.
        // With a stub ctx (no task dir / no session) it must fail open — never
        // block the subagent — exactly like the Stop arm.
        let mut input = HookInput::default();
        input.extra.insert(
            "agent_type".to_string(),
            serde_json::json!("general-purpose"),
        );
        input.session_id = Some("missing-session".to_string());

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        // Never blocks / never stops the turn — pure observation.
        assert!(output.blocked.is_none());
        assert!(output.continue_.is_none());
    }
}
