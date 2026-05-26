//! `catastrophic_escalation` — PreToolUse hook that intercepts
//! Catastrophic-class tool calls and surfaces them upstream via
//! the existing legatus escalation path.
//!
//! # Scope (sentinel/legatus-side communication seam only)
//!
//! When the reversibility classifier returns
//! [`ReversibilityClass::Catastrophic`] for a tool call, this hook:
//!
//! 1. Fires a fire-and-forget escalation to the daemon-hosted
//!    legatus as `EscalationKind::Blocked{BlockReason::Custom}`.
//!    The legatus forwards it over the Consular Protocol as a
//!    `SessionBlocked` envelope; the consulate routes it to the
//!    consul that owns this session for voice-attested
//!    authorization. (Voice / Praefectus / witness production is
//!    consul-side concerns; this hook only signals.)
//! 2. Returns [`HookOutput::deny`] with an operator-facing message
//!    so Claude Code halts the tool call locally pending the
//!    operator's upstream authorization.
//!
//! This is the **trigger half** of the catastrophic flow. The
//! receive-ack-and-resume half (inbound `CatastrophicAck` handling
//! in sentinel-legatus + daemon-held approval cache so a retry
//! sees the operator's approval) is a follow-up commit.
//!
//! # Why a separate hook from `tool_usage_gate`
//!
//! `tool_usage_gate` already classifies tool calls and decides
//! local-gate behavior (TriviallyReversible -> allow,
//! ReversibleWithEffort -> four-check stack,
//! Irreversible/Catastrophic -> defer to A3 auditor or fall
//! through). It owns the local-blast-radius decision tree.
//!
//! This hook owns the orthogonal upstream-signaling decision: any
//! Catastrophic-class call gets signaled to the operator's consul
//! regardless of which local gate runs after. Keeping the
//! responsibilities separate means tool_usage_gate doesn't need to
//! grow daemon-talking-to behavior, and the upstream signaling can
//! evolve (e.g. become BlockReason::CatastrophicPending once the
//! consul-protocol variant lands) without churning tool_usage_gate.
//!
//! Both hooks fire on PreToolUse and merge their HookOutputs in
//! hook_cmd.rs.

#![allow(clippy::missing_errors_doc, clippy::doc_markdown)]

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::ReversibilityClassifierPort;
use sentinel_domain::reversibility::ReversibilityClass;
use sentinel_legatus::{BlockReason, EscalationKind};

use crate::legatus_client::{consume_catastrophic_approval, escalate_fire_and_forget};

/// Decouples the hook from the daemon HTTP call so tests can
/// inject a fixed-result checker.
pub trait CatastrophicApprovalChecker {
    /// Returns `true` when an unspent approval is present for
    /// `(session_id, action_class)`. Implementations consume the
    /// approval on a hit.
    fn check(&self, session_id: &str, action_class: &str) -> bool;
}

/// Production checker: hits the daemon's
/// `/legatus/catastrophic-acks/:session_id/:action_class` route.
/// Fails-closed on daemon outage.
pub struct DaemonApprovalChecker;

impl CatastrophicApprovalChecker for DaemonApprovalChecker {
    fn check(&self, session_id: &str, action_class: &str) -> bool {
        consume_catastrophic_approval(session_id, action_class)
    }
}

/// Maximum length of the operator-facing instruction-content
/// summary embedded in the BlockReason::Custom description.
/// Conservative cap to keep notification payloads small.
const SUMMARY_MAX_LEN: usize = 120;

/// Process a PreToolUse event.
///
/// Decision order:
///   1. If no tool_name -> allow.
///   2. If not Catastrophic -> allow (other hooks own those
///      classes).
///   3. If Catastrophic AND the operator has a fresh unspent
///      approval for `(session_id, action_class)` in the daemon's
///      cache -> mark spent + allow. This is the retry-allow path
///      that closes the catastrophic loop on the sentinel side.
///   4. Else (Catastrophic + no approval) -> emit SessionBlocked
///      upstream + deny locally. Operator approves via voice
///      surface; CatastrophicAck arrives at the daemon; the
///      operator's NEXT prompt triggers a retry that hits the
///      allow-path above.
pub fn process(
    input: &HookInput,
    classifier: &dyn ReversibilityClassifierPort,
    approval_checker: &dyn CatastrophicApprovalChecker,
) -> HookOutput {
    let Some(tool) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };
    let null_input = serde_json::Value::Null;
    let tool_input = input.tool_input.as_ref().unwrap_or(&null_input);

    let class = classifier.classify(tool, tool_input);
    if class != ReversibilityClass::Catastrophic {
        return HookOutput::allow();
    }

    // Catastrophic. Try to consume an existing approval first.
    // session_id can be absent in synthetic / test inputs; without
    // it we cannot look up an approval, so fall through to emit +
    // deny.
    if let Some(session_id) = input.session_id.as_deref() {
        if approval_checker.check(session_id, tool) {
            tracing::info!(
                target: "sentinel::catastrophic_escalation",
                tool = %tool,
                session_id = %session_id,
                "consumed pre-recorded CatastrophicAck approval; allowing this retry"
            );
            return HookOutput::allow();
        }
    }

    // No approval -- emit upstream + deny locally.
    // Use the dedicated BlockReason::CatastrophicPending variant
    // (consul-protocol just landed it) so consul-side routing can
    // pattern-match on the structured signal instead of parsing
    // free-text out of BlockReason::Custom. action_class is the
    // tool name -- matches what the operator says in
    // "approve <action_class>, code <nonce>".
    let action_summary = render_summary(tool_input);
    let description = render_description(tool, tool_input);
    tracing::warn!(
        target: "sentinel::catastrophic_escalation",
        tool = %tool,
        "Catastrophic-class tool call intercepted; \
         emitting SessionBlocked{{CatastrophicPending}} upstream and denying locally"
    );
    escalate_fire_and_forget(EscalationKind::Blocked {
        reason: BlockReason::CatastrophicPending {
            action_class: tool.to_string(),
            action_summary,
        },
    });
    HookOutput::deny(format!(
        "Catastrophic-class action requires voice-attested authorization from the \
         operator. Sentinel has signaled the consul; await approval. ({description})"
    ))
}

/// Render just the action-content summary (without the tool name
/// prefix) for the CatastrophicPending::action_summary field. The
/// tool name is carried separately in action_class, so the
/// summary doesn't need to repeat it.
fn render_summary(tool_input: &serde_json::Value) -> String {
    let raw = extract_summary(tool_input);
    if raw.len() > SUMMARY_MAX_LEN {
        format!("{}...", &raw[..SUMMARY_MAX_LEN.saturating_sub(3)])
    } else {
        raw
    }
}

/// Render a short operator-facing description from the tool name +
/// input. The full input may be large or sensitive; this hook is
/// downstream of the audit log so verbatim content is recorded
/// there. The notification needs a glance-readable summary, so we
/// pull common content fields (command/path/content/instruction)
/// out of the tool_input object and truncate.
fn render_description(tool_name: &str, tool_input: &serde_json::Value) -> String {
    let summary = extract_summary(tool_input);
    let summary = if summary.is_empty() {
        String::new()
    } else {
        let trimmed = if summary.len() > SUMMARY_MAX_LEN {
            format!("{}...", &summary[..SUMMARY_MAX_LEN.saturating_sub(3)])
        } else {
            summary
        };
        format!(": {trimmed}")
    };
    format!("{tool_name}{summary}")
}

/// Best-effort extract of a content summary from common tool-input
/// shapes. Each branch is a structural match against the well-
/// known input fields the Sentinel codebase already consumes; any
/// unmatched shape falls through to empty string (the description
/// is just `tool_name` in that case).
fn extract_summary(tool_input: &serde_json::Value) -> String {
    let Some(obj) = tool_input.as_object() else {
        return String::new();
    };
    for key in &[
        "command",
        "instruction",
        "file_path",
        "path",
        "content",
        "url",
    ] {
        if let Some(v) = obj.get(*key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                // Strip newlines so the notification is one-line.
                return v.split_whitespace().collect::<Vec<_>>().join(" ");
            }
        }
    }
    String::new()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Stand-in classifier that returns a configured class
    /// regardless of input. Mirrors the test pattern in
    /// tool_usage_gate.
    #[derive(Clone, Copy)]
    struct FixedClassifier(ReversibilityClass);

    impl ReversibilityClassifierPort for FixedClassifier {
        fn classify(
            &self,
            _tool_name: &str,
            _tool_input: &serde_json::Value,
        ) -> ReversibilityClass {
            self.0
        }
    }

    /// Approval checker that always returns the configured value.
    /// Models a daemon that always has / never has a pending
    /// approval.
    #[derive(Clone, Copy)]
    struct FixedApproval(bool);

    impl CatastrophicApprovalChecker for FixedApproval {
        fn check(&self, _session_id: &str, _action_class: &str) -> bool {
            self.0
        }
    }

    const NEVER_APPROVE: FixedApproval = FixedApproval(false);
    const ALWAYS_APPROVE: FixedApproval = FixedApproval(true);

    fn input_with(tool: &str, tool_input: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool.into()),
            tool_input: Some(tool_input),
            ..Default::default()
        }
    }

    #[test]
    fn catastrophic_tool_call_denies_locally() {
        let input = input_with("Bash", json!({"command": "rm -rf /"}));
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::Catastrophic),
            &NEVER_APPROVE,
        );
        // The hook denies; the operator-facing message names the
        // tool and the action.
        assert_eq!(r.blocked, Some(true), "expected deny");
        let hso = r
            .hook_specific_output
            .as_ref()
            .expect("deny carries hookSpecificOutput");
        assert!(
            hso.permission_decision_reason.is_some(),
            "deny should carry a permission_decision_reason"
        );
    }

    #[test]
    fn irreversible_tool_call_allows() {
        let input = input_with("Bash", json!({"command": "git push origin main"}));
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::Irreversible),
            &NEVER_APPROVE,
        );
        assert!(
            r.blocked != Some(true),
            "expected allow, got blocked={:?}",
            r.blocked
        );
    }

    #[test]
    fn reversible_with_effort_allows() {
        let input = input_with("Edit", json!({"file_path": "/tmp/foo.rs"}));
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::ReversibleWithEffort),
            &NEVER_APPROVE,
        );
        assert!(
            r.blocked != Some(true),
            "expected allow, got blocked={:?}",
            r.blocked
        );
    }

    #[test]
    fn trivially_reversible_allows() {
        let input = input_with("Read", json!({"file_path": "/tmp/foo.rs"}));
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::TriviallyReversible),
            &NEVER_APPROVE,
        );
        assert!(
            r.blocked != Some(true),
            "expected allow, got blocked={:?}",
            r.blocked
        );
    }

    #[test]
    fn missing_tool_name_allows() {
        let input = HookInput::default();
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::Catastrophic),
            &NEVER_APPROVE,
        );
        assert!(
            r.blocked != Some(true),
            "expected allow, got blocked={:?}",
            r.blocked
        );
    }

    #[test]
    fn description_pulls_command_field() {
        let s = render_description("Bash", &json!({"command": "DROP TABLE users"}));
        assert!(s.contains("Bash"));
        assert!(s.contains("DROP TABLE users"));
    }

    #[test]
    fn description_pulls_file_path_field() {
        let s = render_description("Edit", &json!({"file_path": "/etc/passwd"}));
        assert!(s.contains("Edit"));
        assert!(s.contains("/etc/passwd"));
    }

    #[test]
    fn description_truncates_long_content() {
        let big = "a".repeat(500);
        let s = render_description("Write", &json!({"content": big}));
        assert!(
            s.len() < 200,
            "description should truncate, got {}",
            s.len()
        );
        assert!(s.ends_with("..."), "should mark truncation, got: {s}");
    }

    #[test]
    fn description_for_unknown_shape_is_just_tool_name() {
        let s = render_description("UnknownTool", &json!({"random": 42}));
        assert_eq!(s, "UnknownTool");
    }

    #[test]
    fn description_strips_internal_whitespace_in_summary() {
        let s = render_description("Bash", &json!({"command": "echo\n\n  hello\t\tworld"}));
        // Multiple whitespace runs collapsed into single spaces.
        assert!(s.contains("echo hello world"), "got: {s}");
    }

    #[test]
    fn description_handles_null_input() {
        let s = render_description("Bash", &serde_json::Value::Null);
        assert_eq!(s, "Bash");
    }

    #[test]
    fn catastrophic_message_mentions_authorization() {
        let input = input_with("Bash", json!({"command": "drop database prod"}));
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::Catastrophic),
            &NEVER_APPROVE,
        );
        let hso = r
            .hook_specific_output
            .as_ref()
            .expect("deny carries hookSpecificOutput");
        let msg = hso.permission_decision_reason.as_deref().unwrap_or("");
        assert!(
            msg.to_lowercase().contains("authorization"),
            "deny message should mention authorization, got: {msg}"
        );
    }

    /// Retry-allow path: when the daemon's cache reports an
    /// existing approval for (session, action_class), the hook
    /// allows the retry without re-emitting SessionBlocked.
    #[test]
    fn catastrophic_with_pending_approval_allows_retry() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "rm -rf /var/log/old"})),
            session_id: Some("11111111-2222-3333-4444-555555555555".into()),
            ..Default::default()
        };
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::Catastrophic),
            &ALWAYS_APPROVE,
        );
        assert!(
            r.blocked != Some(true),
            "approved retry should allow, got blocked={:?}",
            r.blocked
        );
    }

    /// Without session_id we cannot look up an approval (the
    /// daemon keys by session). Catastrophic + no session_id +
    /// even an ALWAYS_APPROVE checker -> still deny, because the
    /// hook short-circuits the approval check.
    #[test]
    fn catastrophic_without_session_id_skips_approval_check_and_denies() {
        let input = HookInput {
            tool_name: Some("Bash".into()),
            tool_input: Some(json!({"command": "rm -rf /"})),
            session_id: None,
            ..Default::default()
        };
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::Catastrophic),
            &ALWAYS_APPROVE,
        );
        assert_eq!(
            r.blocked,
            Some(true),
            "without session_id we cannot consume approval; \
             must deny + emit so the operator re-authorizes"
        );
    }

    /// Approval check is only consulted for Catastrophic. For
    /// other classes the hook silently allows regardless of cache
    /// state (other hooks own those classes).
    #[test]
    fn approval_check_not_consulted_for_non_catastrophic() {
        struct ExplodingChecker;
        impl CatastrophicApprovalChecker for ExplodingChecker {
            fn check(&self, _session_id: &str, _action_class: &str) -> bool {
                panic!("approval checker should not be consulted for non-Catastrophic class");
            }
        }
        let input = HookInput {
            tool_name: Some("Edit".into()),
            tool_input: Some(json!({"file_path": "/tmp/foo.rs"})),
            session_id: Some("11111111-2222-3333-4444-555555555555".into()),
            ..Default::default()
        };
        let r = process(
            &input,
            &FixedClassifier(ReversibilityClass::ReversibleWithEffort),
            &ExplodingChecker,
        );
        assert!(r.blocked != Some(true));
    }
}
