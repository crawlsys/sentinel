//! Agent Revocation Kill Switch
//!
//! AEGIS-borrowed safety control. `PreToolUse` hook that denies any tool
//! call carrying a revoked `agent_id`. Operators trigger revocation via
//! `sentinel agent revoke <id>`; the sentinel CLI mutates [`SessionState::revoke_agent`]
//! and this hook reads from it on every subsequent tool call.
//!
//! # Threat model
//!
//! "An agent has gone off the rails — kill its ability to act, immediately."
//! Examples:
//! - A subagent loop is firing the same destructive tool repeatedly
//! - A teammate in an agent-team has started doing work outside its
//!   declared task
//! - A specific spawned agent is producing tool calls the operator
//!   doesn't recognize
//!
//! Revocation is **agent-id scoped**, not session-wide. Other agents in
//! the same session keep working; this hook is the surgical version of
//! "kill everything".
//!
//! # Provenance
//!
//! Denials are tagged with `[Sentinel-Authority]` so Claude Code's
//! runtime hard-rejects them — the agent literally cannot proceed.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::SessionState;

/// Process a `PreToolUse` event. Returns:
/// - [`HookOutput::allow`] when no `agent_id` is present, or when the
///   `agent_id` isn't on the revocation list.
/// - [`HookOutput::deny`] when the `agent_id` is revoked. The deny
///   message names the revoked `agent_id` so the user has unambiguous
///   feedback in tool-result text.
pub fn process(input: &HookInput, state: &SessionState) -> HookOutput {
    let agent_id = match input.agent_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        // No agent_id on the input = main session, never revokable.
        // Revocation is for spawned subagents/teammates only.
        _ => return HookOutput::allow(),
    };

    if !state.is_agent_revoked(agent_id) {
        return HookOutput::allow();
    }

    HookOutput::deny(super::block_context::append_block_context(
        format!(
            "[Sentinel-Authority] agent_revocation: agent '{agent_id}' has been \
             revoked for this session. Tool calls from this agent are refused. \
             Lift the revocation via `sentinel agent unrevoke {agent_id}` if \
             this was a mistake; otherwise the agent must terminate.",
        ),
        input,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::events::PermissionDecision;

    fn input_with_agent(agent_id: Option<&str>) -> HookInput {
        HookInput {
            tool_name: Some("Bash".into()),
            agent_id: agent_id.map(|s| s.to_string()),
            ..HookInput::default()
        }
    }

    fn is_deny(out: &HookOutput) -> bool {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision)
            == Some(PermissionDecision::Deny)
    }

    fn is_allow(out: &HookOutput) -> bool {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision)
            .is_none()
    }

    #[test]
    fn allow_when_no_agent_id() {
        let state = SessionState::new("test");
        let out = process(&input_with_agent(None), &state);
        assert!(is_allow(&out), "no agent_id = main session = always allow");
    }

    #[test]
    fn allow_when_empty_agent_id() {
        let state = SessionState::new("test");
        let out = process(&input_with_agent(Some("")), &state);
        assert!(is_allow(&out), "empty agent_id treated as no agent_id");
    }

    #[test]
    fn allow_when_agent_id_not_revoked() {
        let state = SessionState::new("test");
        let out = process(&input_with_agent(Some("agent-abc")), &state);
        assert!(is_allow(&out), "non-revoked agent must pass");
    }

    #[test]
    fn deny_when_agent_id_is_revoked() {
        let mut state = SessionState::new("test");
        state.revoke_agent("agent-abc");
        let out = process(&input_with_agent(Some("agent-abc")), &state);
        assert!(is_deny(&out), "revoked agent must be denied");
        let reason = out
            .hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref())
            .unwrap();
        assert!(
            reason.contains("[Sentinel-Authority]"),
            "deny carries provenance prefix",
        );
        assert!(
            reason.contains("agent-abc"),
            "deny names the revoked agent_id, got: {reason}",
        );
    }

    #[test]
    fn revocation_is_agent_scoped_not_session_wide() {
        // Two different agent_ids — revoking one must NOT block the other.
        let mut state = SessionState::new("test");
        state.revoke_agent("agent-bad");

        // Revoked agent: denied.
        let denied = process(&input_with_agent(Some("agent-bad")), &state);
        assert!(is_deny(&denied));

        // Different agent in the same session: allowed.
        let allowed = process(&input_with_agent(Some("agent-good")), &state);
        assert!(is_allow(&allowed));
    }

    #[test]
    fn unrevoke_lifts_the_block() {
        let mut state = SessionState::new("test");
        state.revoke_agent("agent-x");
        assert!(is_deny(&process(
            &input_with_agent(Some("agent-x")),
            &state
        )));

        let removed = state.unrevoke_agent("agent-x");
        assert!(
            removed,
            "unrevoke returned true for previously-revoked agent"
        );

        assert!(
            is_allow(&process(&input_with_agent(Some("agent-x")), &state)),
            "post-unrevoke calls must pass",
        );
    }

    #[test]
    fn unrevoke_returns_false_for_unknown_agent() {
        let mut state = SessionState::new("test");
        let removed = state.unrevoke_agent("never-revoked");
        assert!(!removed, "unrevoke returns false when nothing was revoked");
    }

    #[test]
    fn revoke_is_idempotent() {
        let mut state = SessionState::new("test");
        state.revoke_agent("agent-y");
        state.revoke_agent("agent-y"); // duplicate
        assert!(state.is_agent_revoked("agent-y"));
        // Single unrevoke fully clears (HashSet semantics — not a counter).
        assert!(state.unrevoke_agent("agent-y"));
        assert!(!state.is_agent_revoked("agent-y"));
    }
}
