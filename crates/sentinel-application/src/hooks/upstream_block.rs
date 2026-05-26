//! Shared helper that wraps local hook denials with an upstream
//! `SessionBlocked` signal.
//!
//! # Why
//!
//! Several `PreToolUse` hooks block a tool call when their gate
//! conditions aren't met (`phase_gate` requires plan-mode for
//! risky ops, `pre_commit_verification` requires a verification
//! token before commits, `git_hygiene` blocks Edit/Write past the
//! uncommitted-files threshold). Before this helper, those blocks
//! stayed local -- the operator interacting with the agent over a
//! remote surface (Telegram, voice, web-agent) saw no signal that
//! their agent was stuck.
//!
//! This helper fires an `EscalationKind::Blocked{Custom}` via the
//! daemon-hosted legatus alongside the local Deny/Block so the
//! operator's consul gets a notification. Fire-and-forget; daemon
//! outage is a silent no-op.
//!
//! # Scope (sentinel/legatus-side communication seam)
//!
//! Per the architectural boundary: voice / Praefectus / consul UX
//! all consul-side. Sentinel signals; consul presents.

#![allow(clippy::missing_errors_doc)]

use sentinel_domain::events::HookOutput;
use sentinel_legatus::{BlockReason, EscalationKind};

use crate::legatus_client::escalate_fire_and_forget;

/// Fire-and-forget a `SessionBlocked{Custom}` upstream for a
/// local hook block. `hook_name` tags the description so the
/// operator sees which gate fired; `reason` is the operator-
/// facing message.
pub fn signal_upstream(hook_name: &str, reason: &str) {
    let description = render_description(hook_name, reason);
    escalate_fire_and_forget(EscalationKind::Blocked {
        reason: BlockReason::Custom { description },
    });
}

/// Convenience wrapper around `HookOutput::deny`: fires upstream
/// AND returns the deny output. Use at call sites that already
/// returned `HookOutput::deny(reason)`.
#[must_use]
pub fn deny_with_upstream(hook_name: &str, reason: String) -> HookOutput {
    signal_upstream(hook_name, &reason);
    HookOutput::deny(reason)
}

/// Convenience wrapper around `HookOutput::block`: same shape as
/// `deny_with_upstream` but routes through the `block` variant
/// (used by hooks like `pre_commit_verification` that emit a
/// generic block rather than a `PreToolUse` deny).
#[must_use]
pub fn block_with_upstream(hook_name: &str, reason: String) -> HookOutput {
    signal_upstream(hook_name, &reason);
    HookOutput::block(reason)
}

/// Cap the embedded reason at this many chars to keep the
/// notification payload small. The local Deny/Block message
/// carries the full text.
const REASON_MAX_LEN: usize = 200;

fn render_description(hook_name: &str, reason: &str) -> String {
    let one_line: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = if one_line.len() > REASON_MAX_LEN {
        format!("{}...", &one_line[..REASON_MAX_LEN.saturating_sub(3)])
    } else {
        one_line
    };
    format!("local block: {hook_name}: {trimmed}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn render_description_strips_whitespace_and_caps_length() {
        let big = "word ".repeat(200);
        let s = render_description("git_hygiene", &big);
        assert!(s.starts_with("local block: git_hygiene: "));
        assert!(s.len() < 300, "should truncate, got {} chars", s.len());
        assert!(s.ends_with("..."), "should mark truncation");
    }

    #[test]
    fn render_description_normalizes_newlines() {
        let r = "first line\n\nsecond  line\twith\ttabs";
        let s = render_description("phase_gate", r);
        assert!(s.contains("first line second line with tabs"));
    }

    #[test]
    fn render_description_short_reason_is_passed_through() {
        let s = render_description("pre_commit", "needs verification");
        assert_eq!(s, "local block: pre_commit: needs verification");
    }

    #[test]
    fn deny_with_upstream_returns_blocked_output() {
        let r = deny_with_upstream("hook_x", "you shall not pass".into());
        assert_eq!(r.blocked, Some(true));
    }

    #[test]
    fn block_with_upstream_returns_blocked_output() {
        let r = block_with_upstream("hook_y", "verification required".into());
        assert_eq!(r.blocked, Some(true));
    }
}
