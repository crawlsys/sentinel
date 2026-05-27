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

/// Phrase that arms session-wide prod work.
const ARM_PHRASE: &str = "production override";
/// Phrase that re-locks prod work.
const LOCK_PHRASE: &str = "production lock";

/// Max length of the line bearing the arm phrase for it to count as a
/// deliberate operator command. **Injection hardening:** a real arming is a
/// short command-like utterance ("production override — hotfix the auth
/// migration" ≈ 50 chars); the phrase buried inside a long pasted log line,
/// fetched web snippet, or file blob is almost certainly NOT the operator
/// intending to arm prod. Gating arm on a short phrase-line meaningfully
/// reduces accidental/injected arming while honoring the exact phrase Gary
/// chose. It does NOT eliminate the risk (a short pasted line could still
/// match) — the dual-display notice is the real backstop: Gary SEES the arm
/// and can immediately "production lock". Lock is deliberately NOT length-
/// gated (locking is fail-safe toward refusal, so it should trigger easily).
const MAX_ARM_LINE_LEN: usize = 120;

/// Does the prompt arm prod work? True only when the arm phrase appears on a
/// short, command-like line (see [`MAX_ARM_LINE_LEN`]) — not merely anywhere
/// in the text. Case-insensitive.
#[must_use]
pub fn is_arm(prompt_lower: &str) -> bool {
    prompt_lower.lines().any(|line| {
        let t = line.trim();
        t.contains(ARM_PHRASE) && t.chars().count() <= MAX_ARM_LINE_LEN
    })
}

/// Does the prompt contain the lock phrase anywhere? Not length-gated —
/// locking returns to the safe (refusal) posture, so it should fire easily.
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
        // Capture the short command-like line that armed it as the note
        // (same gating as is_arm, so we record the operator's actual command,
        // not some unrelated long line that also happens to contain the phrase).
        let note = prompt
            .lines()
            .map(str::trim)
            .find(|l| {
                l.to_lowercase().contains(ARM_PHRASE) && l.chars().count() <= MAX_ARM_LINE_LEN
            })
            .map(ToString::to_string);
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

    #[test]
    fn phrase_buried_in_a_long_pasted_line_does_not_arm() {
        // Injection hardening: the phrase inside a long log/paste line is not a
        // deliberate arm. One long line > MAX_ARM_LINE_LEN containing the phrase.
        let mut state = SessionState::new("s");
        let long_line = format!(
            "2026-05-26T12:00:00Z ERROR deploy log: the runbook says to request a \
             production override from the on-call before touching anything {}",
            "x".repeat(80)
        );
        assert!(long_line.chars().count() > MAX_ARM_LINE_LEN);
        let out = process(&prompt_input(&long_line), &mut state);
        assert!(
            !state.production_override_armed(),
            "phrase buried in a long line must NOT arm"
        );
        assert!(out.system_message.is_none());
    }

    #[test]
    fn phrase_on_a_short_command_line_arms_even_inside_a_longer_prompt() {
        // A short command line arms even if the prompt has other (short) lines.
        let mut state = SessionState::new("s");
        let prompt = "hey can you\nproduction override\nthen run the migration";
        process(&prompt_input(prompt), &mut state);
        assert!(state.production_override_armed());
    }

    #[test]
    fn lock_fires_even_in_a_long_line() {
        // Lock is fail-safe: it should re-lock regardless of line length.
        let mut state = SessionState::new("s");
        state.arm_production_override(None);
        let long = format!("{} production lock {}", "y".repeat(100), "z".repeat(100));
        process(&prompt_input(&long), &mut state);
        assert!(!state.production_override_armed(), "lock must fire even in a long line");
    }
}
