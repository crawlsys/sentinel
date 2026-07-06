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
/// Resolve the completing agent's `(agent_type, agent_id)` from a
/// `SubagentStop` payload.
///
/// Claude Code sends `agent_type` and `agent_id` as TOP-LEVEL SubagentStop
/// fields (per code.claude.com/docs), which serde deserializes into the typed
/// `HookInput` fields. The pre-fix code read `input.extra["agent_type"]`, but a
/// value captured by a named struct field is never *also* placed in the
/// `#[serde(flatten)]` `extra` map — so that read was always empty and every
/// event said "(unknown)". We read the typed field first and fall back to
/// `extra` only for resilience against a future harness that relocates it.
/// Empty strings are treated as absent. `agent_type` defaults to `"unknown"`;
/// `agent_id` stays `None` when unavailable.
fn resolve_agent_identity(input: &HookInput) -> (&str, Option<&str>) {
    fn typed_or_extra<'a>(
        typed: Option<&'a str>,
        extra: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<&'a str> {
        typed.filter(|s| !s.is_empty()).or_else(|| {
            extra
                .get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
        })
    }
    let agent_type =
        typed_or_extra(input.agent_type.as_deref(), &input.extra, "agent_type").unwrap_or("unknown");
    let agent_id = typed_or_extra(input.agent_id.as_deref(), &input.extra, "agent_id");
    (agent_type, agent_id)
}

pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let (agent_type, agent_id) = resolve_agent_identity(input);

    tracing::debug!(agent_type, agent_id, "Subagent stopping");

    // Emit channel event for real-time push notification. Include the agent id
    // (when present) in the summary so concurrent agents of the same type are
    // distinguishable, and stash both fields in meta for consumers.
    let summary = match agent_id {
        Some(id) => format!("Agent \"{agent_type}\" ({id}) has finished."),
        None => format!("Agent \"{agent_type}\" has finished."),
    };
    let mut meta = serde_json::Map::new();
    meta.insert(
        "agent_type".to_string(),
        serde_json::Value::String(agent_type.to_string()),
    );
    if let Some(id) = agent_id {
        meta.insert(
            "agent_id".to_string(),
            serde_json::Value::String(id.to_string()),
        );
    }
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
    fn resolve_identity_reads_typed_fields() {
        // The real Claude Code payload shape: agent_type/agent_id land in the
        // typed HookInput fields, NOT in `extra`. This is the case the old code
        // got wrong (always "unknown").
        let mut input = HookInput::default();
        input.agent_type = Some("Explore".to_string());
        input.agent_id = Some("agent-42".to_string());
        let (ty, id) = resolve_agent_identity(&input);
        assert_eq!(ty, "Explore");
        assert_eq!(id, Some("agent-42"));
    }

    #[test]
    fn resolve_identity_defaults_to_unknown_when_absent() {
        let input = HookInput::default();
        let (ty, id) = resolve_agent_identity(&input);
        assert_eq!(ty, "unknown");
        assert_eq!(id, None);
    }

    #[test]
    fn resolve_identity_typed_field_beats_extra() {
        // Typed field is authoritative; a stale `extra` copy does not override.
        let mut input = HookInput::default();
        input.agent_type = Some("code-reviewer".to_string());
        input
            .extra
            .insert("agent_type".to_string(), serde_json::json!("stale-debugger"));
        let (ty, _) = resolve_agent_identity(&input);
        assert_eq!(ty, "code-reviewer");
    }

    #[test]
    fn resolve_identity_falls_back_to_extra_for_resilience() {
        // If a future harness only puts it in `extra`, we still read it.
        let mut input = HookInput::default();
        input
            .extra
            .insert("agent_type".to_string(), serde_json::json!("Plan"));
        input
            .extra
            .insert("agent_id".to_string(), serde_json::json!("x-7"));
        let (ty, id) = resolve_agent_identity(&input);
        assert_eq!(ty, "Plan");
        assert_eq!(id, Some("x-7"));
    }

    #[test]
    fn resolve_identity_empty_string_is_treated_as_absent() {
        let mut input = HookInput::default();
        input.agent_type = Some(String::new());
        let (ty, _) = resolve_agent_identity(&input);
        assert_eq!(ty, "unknown");
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
