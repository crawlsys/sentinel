//! Glass-Break Gate Helper
//!
//! Shared predicate that lets enforcement gates honor a first-class
//! [`GlassBreak`] (the audited override initiated by `sentinel break`),
//! in addition to the coarse phrase-based [`super::hygiene_override`].
//!
//! ## Why this exists
//!
//! `sentinel break` writes a [`GlassBreak`] into the session's
//! [`SessionState::glass_break`] field — a fully audited override with a
//! 6-digit anti-AI-self-invocation challenge, a reason, a duration, an
//! optional `workflow` scope, rate limiting, and a JSONL audit log. The
//! [`phase_gate`](super::phase_gate) and [`step_gate`](super::step_gate)
//! hooks already consult it via `state.is_break_active()`. The other
//! enforcement gates (`git_hygiene`, `pre_commit_verification`, and the
//! `verification_gate` reminder) historically only honored the coarse
//! `hygiene_override`, so the audited first-class override didn't actually
//! suppress them.
//!
//! [`active_glass_break`] closes that gap. It reads the **same in-memory
//! [`SessionState`]** the dispatcher already loaded and that
//! `phase_gate`/`step_gate` consult — no new state-loading path is
//! introduced.
//!
//! ## Workflow scoping
//!
//! A [`GlassBreak`] may carry an optional `workflow` tag (e.g. `git_hygiene`,
//! `verification`). When set, the break only suppresses gates that pass the
//! matching `workflow` argument. When `None`, the break is unscoped and
//! covers every gate.

use sentinel_domain::state::SessionState;

/// Returns `true` when an active (non-expired) [`GlassBreak`] should suppress
/// the gate identified by `workflow`.
///
/// Matching rules (mirrors the `phase_gate` / `step_judge` semantics):
/// - There must be a [`GlassBreak`] present and the current time must be
///   before its `expires_at` (i.e. [`SessionState::is_break_active`]).
/// - If the break carries a `workflow` scope, it only matches when that scope
///   equals the requested `workflow`. A `None` scope covers all gates.
///
/// On a match, a single audit line is emitted to stderr so the suppression is
/// observable alongside the JSONL break log written by `sentinel break`.
#[must_use]
pub fn active_glass_break(state: &SessionState, workflow: &str) -> bool {
    let Some(gb) = state.glass_break.as_ref() else {
        return false;
    };
    if !state.is_break_active() {
        return false;
    }
    // Scope check: a break tagged with a specific workflow only suppresses
    // that workflow's gate; an untagged (None) break covers everything.
    if let Some(scope) = gb.workflow.as_deref() {
        if scope != workflow {
            return false;
        }
    }
    eprintln!(
        "[sentinel][glass_break] {workflow} gate suppressed by active glass break (reason: {})",
        gb.reason
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use sentinel_domain::state::GlassBreak;

    fn break_with(workflow: Option<&str>, minutes_remaining: i64) -> GlassBreak {
        let now = Utc::now();
        GlassBreak {
            reason: "emergency hotfix".to_string(),
            started_at: now - Duration::minutes(1),
            expires_at: now + Duration::minutes(minutes_remaining),
            duration_minutes: 5,
            workflow: workflow.map(str::to_string),
            challenge_code: "BREAK-123456".to_string(),
            tools_used: Vec::new(),
        }
    }

    #[test]
    fn no_break_present_is_false() {
        let state = SessionState::new("sess-gb-none");
        assert!(!active_glass_break(&state, "git_hygiene"));
    }

    #[test]
    fn active_unscoped_break_covers_all_gates() {
        let mut state = SessionState::new("sess-gb-unscoped");
        state.glass_break = Some(break_with(None, 5));
        assert!(active_glass_break(&state, "git_hygiene"));
        assert!(active_glass_break(&state, "verification"));
        assert!(active_glass_break(&state, "anything-else"));
    }

    #[test]
    fn expired_break_is_false() {
        let mut state = SessionState::new("sess-gb-expired");
        // Negative remaining minutes → already expired.
        state.glass_break = Some(break_with(None, -1));
        assert!(!active_glass_break(&state, "git_hygiene"));
    }

    #[test]
    fn workflow_scoped_break_only_matches_its_scope() {
        let mut state = SessionState::new("sess-gb-scoped");
        state.glass_break = Some(break_with(Some("git_hygiene"), 5));
        // Matches the tagged workflow.
        assert!(active_glass_break(&state, "git_hygiene"));
        // Does NOT match a different workflow.
        assert!(!active_glass_break(&state, "verification"));
    }

    #[test]
    fn scoped_but_expired_is_false() {
        let mut state = SessionState::new("sess-gb-scoped-expired");
        state.glass_break = Some(break_with(Some("verification"), -2));
        assert!(!active_glass_break(&state, "verification"));
    }
}
