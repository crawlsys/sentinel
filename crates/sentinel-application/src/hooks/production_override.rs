//! `production_override` — `UserPromptSubmit` hook implementing the
//! operator's session-wide production-override channel.
//!
//! By default the agent refuses production actions (deploys, prod
//! Doppler/Auth0, destructive prod, prod DB ops/migrations). The operator
//! arms prod work for the rest of the session by saying **"production
//! override"**, and re-locks it with **"production lock"**. While armed the
//! agent proceeds on prod work without per-action asking, surfacing each via
//! a dual-display notice.
//!
//! This hook owns the ARM/REVOKE side: it watches each prompt for the two
//! phrases and flips `SessionState.production_override` accordingly, then
//! emits a **dual-display** confirmation through both documented channels —
//! `systemMessage` (Gary's terminal) and `additionalContext` (the model) —
//! mirroring the `memory_inject` capture-notice pattern. The per-action prod
//! notice itself is surfaced by the agent following the policy in the
//! generated CLAUDE.md; this hook is the state machine + the visible
//! arm/disarm signal.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput, HookSpecificOutput};
use sentinel_domain::state::SessionState;

/// Phrase that arms session-wide prod work. Matched case-insensitively as a
/// substring so "production override — hotfix the migration" also arms.
const ARM_PHRASE: &str = "production override";
/// Phrase that re-locks prod work.
const LOCK_PHRASE: &str = "production lock";

/// Does the prompt contain the arm phrase?
#[must_use]
pub fn is_arm(prompt_lower: &str) -> bool {
    prompt_lower.contains(ARM_PHRASE)
}

/// Does the prompt contain the lock phrase?
#[must_use]
pub fn is_lock(prompt_lower: &str) -> bool {
    prompt_lower.contains(LOCK_PHRASE)
}

/// Build the dual-display notice `(human, model)` for an arm/lock transition.
/// `armed == true` is an arm; `false` is a lock.
#[must_use]
pub fn format_transition_notice(armed: bool) -> (String, String) {
    if armed {
        (
            "🔓 PRODUCTION OVERRIDE ARMED — prod actions (deploys, Doppler/Auth0, \
             destructive ops, prod DB) are authorized session-wide. Say \
             \"production lock\" to re-lock."
                .to_string(),
            "[ProductionOverride] ARMED by the operator this session. Production \
             actions are now authorized WITHOUT per-action asking — including prod \
             DB ops/migrations. Proceed, but announce each prod action with a \
             one-line dual-display notice (systemMessage + additionalContext) as \
             you take it. Stays armed until the operator says \"production lock\" \
             or the session ends."
                .to_string(),
        )
    } else {
        (
            "🔒 PRODUCTION LOCK — prod actions are refused again (default posture). \
             Say \"production override\" to re-arm."
                .to_string(),
            "[ProductionOverride] RE-LOCKED by the operator. Production actions are \
             refused again — return to the default posture (ask + require a fresh \
             \"production override\" before any prod action)."
                .to_string(),
        )
    }
}

/// Process a `UserPromptSubmit`. If the prompt arms or locks the production
/// override, flip `state.production_override` and emit a dual-display notice.
/// Lock wins over arm if both phrases somehow appear (fail-safe toward
/// refusal). No phrase → allow unchanged.
#[must_use]
pub fn process(input: &HookInput, state: &mut SessionState) -> HookOutput {
    let Some(prompt) = input.prompt.as_deref() else {
        return HookOutput::allow();
    };
    let lower = prompt.to_lowercase();

    let lock = is_lock(&lower);
    let arm = is_arm(&lower);

    // Lock takes precedence — biasing toward the safe (refusal) state.
    let armed = if lock {
        if !state.production_override_armed() {
            // Locking when already locked is a no-op; don't emit noise.
            return HookOutput::allow();
        }
        state.revoke_production_override();
        false
    } else if arm {
        if state.production_override_armed() {
            // Already armed; re-arming refreshes but we skip the notice to
            // avoid repeating it every prompt that mentions the phrase.
            return HookOutput::allow();
        }
        // Capture the surrounding line as the note (best-effort).
        let note = prompt
            .lines()
            .find(|l| l.to_lowercase().contains(ARM_PHRASE))
            .map(|l| l.trim().to_string());
        state.arm_production_override(note);
        true
    } else {
        return HookOutput::allow();
    };

    let (human, model) = format_transition_notice(armed);
    let mut out = HookOutput::allow();
    out.system_message = Some(human);
    out.hook_specific_output = Some(HookSpecificOutput {
        hook_event_name: HookEvent::UserPromptSubmit.to_string(),
        additional_context: Some(model),
        ..Default::default()
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_input(p: &str) -> HookInput {
        HookInput {
            prompt: Some(p.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn arm_phrase_sets_state_and_emits_dual_display() {
        let mut state = SessionState::new("s");
        assert!(!state.production_override_armed());
        let out = process(&prompt_input("production override — hotfix the migration"), &mut state);
        assert!(state.production_override_armed());
        assert!(out.system_message.is_some(), "human channel set");
        let ctx = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("model channel set");
        assert!(ctx.contains("ARMED"));
        // The surrounding line is captured as the note.
        assert!(state
            .production_override
            .as_ref()
            .and_then(|o| o.note.as_deref())
            .is_some_and(|n| n.contains("hotfix")));
    }

    #[test]
    fn lock_phrase_clears_state() {
        let mut state = SessionState::new("s");
        state.arm_production_override(None);
        let out = process(&prompt_input("production lock"), &mut state);
        assert!(!state.production_override_armed());
        assert!(out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .is_some_and(|c| c.contains("RE-LOCKED")));
    }

    #[test]
    fn lock_wins_over_arm_when_both_present() {
        let mut state = SessionState::new("s");
        state.arm_production_override(None);
        // Both phrases in one prompt → lock (fail-safe toward refusal).
        process(&prompt_input("production override then production lock"), &mut state);
        assert!(!state.production_override_armed());
    }

    #[test]
    fn no_phrase_is_noop() {
        let mut state = SessionState::new("s");
        let out = process(&prompt_input("just deploy the staging build please"), &mut state);
        assert!(!state.production_override_armed());
        assert!(out.system_message.is_none());
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn re_arming_when_already_armed_is_quiet() {
        let mut state = SessionState::new("s");
        state.arm_production_override(None);
        let out = process(&prompt_input("production override again"), &mut state);
        assert!(state.production_override_armed());
        // No repeated notice.
        assert!(out.system_message.is_none());
    }
}
