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

use crate::legatus_client::escalate_fire_and_forget;

/// Maximum length of the operator-facing instruction-content
/// summary embedded in the BlockReason::Custom description.
/// Conservative cap to keep notification payloads small.
const SUMMARY_MAX_LEN: usize = 120;

/// Process a PreToolUse event. Returns Deny when the classifier
/// rules the tool call Catastrophic; returns Allow otherwise.
pub fn process(input: &HookInput, classifier: &dyn ReversibilityClassifierPort) -> HookOutput {
    let Some(tool) = input.tool_name.as_deref() else {
        return HookOutput::allow();
    };
    let null_input = serde_json::Value::Null;
    let tool_input = input.tool_input.as_ref().unwrap_or(&null_input);

    match classifier.classify(tool, tool_input) {
        ReversibilityClass::Catastrophic => {
            let description = render_description(tool, tool_input);
            tracing::warn!(
                target: "sentinel::catastrophic_escalation",
                tool = %tool,
                "Catastrophic-class tool call intercepted; \
                 emitting SessionBlocked upstream and denying locally"
            );
            escalate_fire_and_forget(EscalationKind::Blocked {
                reason: BlockReason::Custom {
                    description: description.clone(),
                },
            });
            HookOutput::deny(format!(
                "Catastrophic-class action requires voice-attested authorization from the \
                 operator. Sentinel has signaled the consul; await approval. ({description})"
            ))
        }
        ReversibilityClass::TriviallyReversible
        | ReversibilityClass::ReversibleWithEffort
        | ReversibilityClass::Irreversible => HookOutput::allow(),
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
        let r = process(&input, &FixedClassifier(ReversibilityClass::Catastrophic));
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
        let r = process(&input, &FixedClassifier(ReversibilityClass::Irreversible));
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
        let r = process(&input, &FixedClassifier(ReversibilityClass::Catastrophic));
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
        let r = process(&input, &FixedClassifier(ReversibilityClass::Catastrophic));
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
}
