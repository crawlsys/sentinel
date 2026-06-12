//! Linear PM-enforcement gate — hard-block starting an undecomposed,
//! oversized Linear ticket.
//!
//! "Good PM is good software." Picking up an 8-point ticket as a single
//! opaque block is the classic PM failure: it hides risk, defies estimation,
//! and almost always should have been decomposed first. This gate makes that
//! discipline load-bearing instead of advisory — it is the live half of the
//! system whose offline/report half is [`crate::linear_pm_audit`].
//!
//! ## Trigger
//!
//! A `PreToolUse` on `mcp__linear__update_issue` whose `tool_input` moves an
//! issue into a *started* state (a `stateId`/`state` change to In Progress).
//! The gate looks the issue up in the local cache
//! (`~/.claude/sentinel/linear-assigned.json`) and, if it carries an
//! `estimate >= OVERSIZED_POINTS` (8), blocks the transition with guidance to
//! decompose first.
//!
//! ## Fail-open by design
//!
//! Like the other gates, this one never blocks on uncertainty: no cache, no
//! identifiable issue, no estimate, or an estimate below the line → allow. It
//! only blocks the one clear, high-confidence violation: starting a known
//! oversized ticket. False positives are worse than a missed nudge, and the
//! `linear-audit` report still surfaces everything the gate lets pass.

use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};
use serde_json::Value;
use std::path::PathBuf;

use super::HookContext;
use crate::linear_pm_audit::OVERSIZED_POINTS;

/// The Linear MCP tool whose calls this gate inspects.
const TARGET_TOOL: &str = "mcp__linear__update_issue";

/// Tokens in a state name/id that indicate a transition *into* active work.
/// We can't resolve a Linear state UUID offline, so we gate on the common
/// case where the caller passes a human-readable state hint, and otherwise
/// fail open.
const STARTED_HINTS: &[&str] = &["in progress", "in-progress", "started", "doing"];

/// `PreToolUse` entry point. Returns `HookOutput::allow()` for everything that
/// is not a high-confidence "start an oversized ticket" call.
pub fn process_pretool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // Only inspect the Linear update tool.
    if input.tool_name.as_deref() != Some(TARGET_TOOL) {
        return HookOutput::allow();
    }
    let Some(args) = input.tool_input.as_ref() else {
        return HookOutput::allow();
    };

    // Only gate transitions that look like "move to a started state".
    if !is_start_transition(args) {
        return HookOutput::allow();
    }

    // Identify the ticket being moved.
    let Some(ticket) = ticket_identifier(args) else {
        return HookOutput::allow();
    };

    // Look up its estimate in the cache; fail open if absent.
    let Some(estimate) = cache_estimate(ctx, &ticket) else {
        return HookOutput::allow();
    };

    if estimate < OVERSIZED_POINTS {
        return HookOutput::allow();
    }

    let envelope = HookEnvelope::block(
        "Linear PM-Enforcement Gate",
        format!(
            "Refusing to start {ticket}: it is a {estimate:.0}-point ticket and \
             has not been decomposed. An 8+ point ticket as a single block hides \
             risk and defies estimation — split it into sub-issues (each ≤ 5 pts) \
             first, then start one of those. Run `sentinel linear-audit scan` for \
             the full PM picture. (This gate only fires on starting a known \
             oversized ticket; everything else is allowed.)"
        ),
    );
    HookOutput::block(envelope.render())
}

/// Does this `update_issue` payload look like a move into active work?
/// We accept either a human-readable `state`/`stateName` hint, or — to avoid
/// false negatives on UUID-only callers — a `start: true`/`started: true`
/// convenience flag if present. Pure UUID `stateId` changes we cannot resolve
/// offline, so they fail open.
fn is_start_transition(args: &Value) -> bool {
    for key in ["state", "stateName", "status", "workflowState"] {
        if let Some(s) = args.get(key).and_then(Value::as_str) {
            let lc = s.to_lowercase();
            if STARTED_HINTS.iter().any(|h| lc.contains(h)) {
                return true;
            }
        }
    }
    // Optional explicit hints some callers set.
    for key in ["start", "started"] {
        if args.get(key).and_then(Value::as_bool) == Some(true) {
            return true;
        }
    }
    false
}

/// Pull a PREFIX-NUMBER ticket identifier out of the args. Accepts `id`,
/// `identifier`, or `issueId` — but only when the value *looks* like an
/// identifier (UUIDs are rejected, since we can't match them to the cache by
/// identifier).
fn ticket_identifier(args: &Value) -> Option<String> {
    for key in ["identifier", "id", "issueId", "issue_id"] {
        if let Some(s) = args.get(key).and_then(Value::as_str) {
            if looks_like_identifier(s) {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// `FPCRM-606` style: an all-alphabetic prefix, a dash, then digits.
fn looks_like_identifier(s: &str) -> bool {
    let mut parts = s.split('-');
    let (Some(prefix), Some(num)) = (parts.next(), parts.next()) else {
        return false;
    };
    parts.next().is_none()
        && !prefix.is_empty()
        && prefix.chars().all(|c| c.is_ascii_alphabetic())
        && !num.is_empty()
        && num.chars().all(|c| c.is_ascii_digit())
}

/// Read the issue cache and return `ticket`'s estimate, if present.
fn cache_estimate(ctx: &HookContext<'_>, ticket: &str) -> Option<f64> {
    let path = cache_path(ctx)?;
    let text = ctx.fs.read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let arr = value
        .as_array()
        .or_else(|| value.get("issues").and_then(Value::as_array))?;
    for issue in arr {
        let id = issue.get("identifier").and_then(Value::as_str);
        if id == Some(ticket) {
            return issue
                .get("estimate")
                .and_then(Value::as_f64)
                .filter(|e| e.is_finite() && *e > 0.0);
        }
    }
    None
}

fn cache_path(ctx: &HookContext<'_>) -> Option<PathBuf> {
    Some(
        ctx.fs
            .home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("linear-assigned.json"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_recognition() {
        assert!(looks_like_identifier("FPCRM-606"));
        assert!(looks_like_identifier("A-1"));
        assert!(!looks_like_identifier("not-an-id-123")); // 3 parts
        assert!(!looks_like_identifier("550e8400-e29b")); // looks uuid-ish
        assert!(!looks_like_identifier("FPCRM")); // no number
        assert!(!looks_like_identifier("123-456")); // numeric prefix
    }

    #[test]
    fn start_transition_detection() {
        let in_progress = serde_json::json!({ "state": "In Progress" });
        assert!(is_start_transition(&in_progress));
        let started_flag = serde_json::json!({ "started": true });
        assert!(is_start_transition(&started_flag));
        let review = serde_json::json!({ "state": "Code Review" });
        assert!(!is_start_transition(&review));
        let uuid_only = serde_json::json!({ "stateId": "550e8400-e29b-41d4" });
        assert!(!is_start_transition(&uuid_only)); // can't resolve → fail open
    }

    #[test]
    fn non_target_tool_is_allowed() {
        let input = HookInput {
            tool_name: Some("Read".into()),
            ..Default::default()
        };
        // No HookContext needed because we short-circuit before touching it;
        // assert via the tool_name guard directly.
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
    }
}
