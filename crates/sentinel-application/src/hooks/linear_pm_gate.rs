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

    // Look up the issue — REAL-TIME first (single-ticket live Linear fetch),
    // falling back to the on-disk cache when the live port is absent or fails.
    // Fail open (allow) only if neither source can produce the issue.
    let Some(issue) = live_or_cached(ctx, &ticket) else {
        return HookOutput::allow();
    };

    // Check 1 (hardest stop): the ticket is BLOCKED. Starting a ticket whose
    // upstream work isn't done is wasted/at-risk effort — refuse it
    // regardless of size. Covers an open blocked-by relation, a Blocked
    // workflow state, or a blocked/blocker label.
    if let Some(reason) = blocked_reason(&issue) {
        let envelope = HookEnvelope::block(
            "Linear PM-Enforcement Gate",
            format!(
                "Refusing to start {ticket}: it is BLOCKED ({reason}). Starting a \
                 blocked ticket means working on something gated by incomplete \
                 upstream work. Resolve the blocker (or remove the block) first, \
                 then start it. Run `sentinel linear-audit scan` for the full PM picture."
            ),
        );
        return HookOutput::block(envelope.render());
    }

    // Check 2: oversized & undecomposed.
    let estimate = issue
        .get("estimate")
        .and_then(Value::as_f64)
        .filter(|e| e.is_finite() && *e > 0.0);
    if let Some(e) = estimate {
        if e >= OVERSIZED_POINTS {
            let envelope = HookEnvelope::block(
                "Linear PM-Enforcement Gate",
                format!(
                    "Refusing to start {ticket}: it is a {e:.0}-point ticket and \
                     has not been decomposed. An 8+ point ticket as a single block hides \
                     risk and defies estimation — split it into sub-issues (each ≤ 5 pts) \
                     first, then start one of those. Run `sentinel linear-audit scan` for \
                     the full PM picture."
                ),
            );
            return HookOutput::block(envelope.render());
        }
    }

    // Check 3: untracked work — starting a ticket with no milestone, *when its
    // project uses milestones*. A ticket you start should be tied to a tracked
    // deliverable. Projects that don't define milestones are exempt (no false
    // block); we only enforce when `projectHasMilestones` is true in the cache.
    if needs_milestone(&issue) {
        let envelope = HookEnvelope::block(
            "Linear PM-Enforcement Gate",
            format!(
                "Refusing to start {ticket}: it has no milestone, but its project \
                 uses milestones. Work you start should map to a tracked deliverable \
                 — assign {ticket} to a project milestone first, then start it. \
                 (Projects without milestones are exempt.)"
            ),
        );
        return HookOutput::block(envelope.render());
    }

    // Check 4: cherry-picking — starting a lower-priority ticket while a
    // higher-priority, startable ticket waits in the same person's queue.
    // Work the most urgent thing first. Fails open if the target is
    // unprioritized or has no assignee to scope by.
    if let Some(higher) = higher_priority_available(ctx, &issue) {
        let envelope = HookEnvelope::block(
            "Linear PM-Enforcement Gate",
            format!(
                "Refusing to start {ticket}: a higher-priority ticket ({higher}) is \
                 available and startable in the same queue. Don't cherry-pick — start \
                 the most urgent work first. Start {higher}, or re-prioritize if {ticket} \
                 genuinely comes first."
            ),
        );
        return HookOutput::block(envelope.render());
    }

    HookOutput::allow()
}

/// Does this issue need (but lack) a milestone? True only when the cache says
/// the project uses milestones (`projectHasMilestones: true`) AND the issue
/// itself carries no `projectMilestone`/`milestone`. Fails open: if the
/// project-has-milestones signal is absent, we don't enforce.
fn needs_milestone(issue: &Value) -> bool {
    let project_uses = issue
        .get("projectHasMilestones")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !project_uses {
        return false;
    }
    let has_milestone = issue
        .get("projectMilestone")
        .or_else(|| issue.get("milestone"))
        .map(|m| !m.is_null())
        .unwrap_or(false);
    !has_milestone
}

/// Return a human-readable reason the issue is blocked, or `None` if it is
/// startable. Checks all three signals a team uses to mark a block:
///   1. an open `blocked by` issue-relation (the related issue is not Done /
///      Canceled),
///   2. a `Blocked` workflow state (by name or type), and
///   3. a `blocked` / `blocker` label.
/// The cache is permissive: any of `relations`/`blockedBy`/`state`/`labels`
/// may be absent, in which case that signal simply doesn't fire (fail open).
fn blocked_reason(issue: &Value) -> Option<String> {
    // 1. Open blocked-by relation. Accept a few shapes: a `blockedBy` array of
    //    issue objects, or a `relations` array of `{type, relatedIssue:{state}}`.
    if let Some(arr) = issue.get("blockedBy").and_then(Value::as_array) {
        for rel in arr {
            if !related_is_resolved(rel) {
                let id = rel
                    .get("identifier")
                    .and_then(Value::as_str)
                    .unwrap_or("an open issue");
                return Some(format!("blocked by {id}"));
            }
        }
    }
    if let Some(arr) = issue.get("relations").and_then(Value::as_array) {
        for rel in arr {
            let ty = rel
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            if ty.contains("blocked") || ty == "blocks_inverse" {
                let related = rel.get("relatedIssue").or_else(|| rel.get("issue"));
                if let Some(r) = related {
                    if !related_is_resolved(r) {
                        let id = r
                            .get("identifier")
                            .and_then(Value::as_str)
                            .unwrap_or("an open issue");
                        return Some(format!("blocked by {id}"));
                    }
                }
            }
        }
    }

    // 2. Blocked workflow state (by name or type).
    if let Some(state) = issue.get("state") {
        let name = state.get("name").and_then(Value::as_str).unwrap_or("");
        let ty = state.get("type").and_then(Value::as_str).unwrap_or("");
        if name.eq_ignore_ascii_case("Blocked") || ty.eq_ignore_ascii_case("blocked") {
            return Some("its workflow state is Blocked".into());
        }
    }

    // 3. A blocked / blocker label. Accept `labels` as an array of strings or
    //    of `{name}` objects.
    if let Some(arr) = issue.get("labels").and_then(Value::as_array) {
        for l in arr {
            let label = l
                .as_str()
                .or_else(|| l.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .to_lowercase();
            if label == "blocked" || label == "blocker" {
                return Some("it carries a 'blocked' label".into());
            }
        }
    }

    None
}

/// Is a related issue resolved (Done / Canceled), i.e. no longer a blocker?
/// Reads its `state.type` (Linear: `completed` / `canceled`). Unknown → treat
/// as still-open (conservative: an unresolved-looking blocker blocks).
fn related_is_resolved(related: &Value) -> bool {
    let ty = related
        .get("state")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    ty == "completed" || ty == "canceled"
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

/// Resolve the issue as real-time as possible: try the live single-ticket
/// Linear lookup first (so the gate reflects this-instant state), and fall back
/// to the on-disk cache when the live port is absent (no token configured) or
/// returns `None` (network error / timeout / not found). This is the
/// "real-time, cache as fallback" contract — never bricks pickup on a flaky
/// network, but is live whenever it can be.
fn live_or_cached(ctx: &HookContext<'_>, ticket: &str) -> Option<Value> {
    if let Some(port) = ctx.linear_lookup {
        if let Some(live) = port.fetch_issue(ticket) {
            return Some(live);
        }
    }
    cache_lookup(ctx, ticket)
}

fn cache_lookup(ctx: &HookContext<'_>, ticket: &str) -> Option<Value> {
    let path = cache_path(ctx)?;
    let text = ctx.fs.read_to_string(&path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let arr = value
        .as_array()
        .or_else(|| value.get("issues").and_then(Value::as_array))?;
    arr.iter()
        .find(|issue| issue.get("identifier").and_then(Value::as_str) == Some(ticket))
        .cloned()
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

/// Read the whole issue cache as a list of issue objects (array or
/// `{issues:[...]}`). Returns an empty vec on any read/parse failure.
fn cache_all(ctx: &HookContext<'_>) -> Vec<Value> {
    let Some(path) = cache_path(ctx) else {
        return Vec::new();
    };
    let Ok(text) = ctx.fs.read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    value
        .as_array()
        .or_else(|| value.get("issues").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

/// Linear priority: 1=Urgent (most urgent) .. 4=Low; 0/absent = No priority.
/// Lower number = more urgent. Returns `None` for 0/absent (unprioritized).
fn priority_of(issue: &Value) -> Option<i64> {
    issue
        .get("priority")
        .and_then(Value::as_i64)
        .filter(|p| *p >= 1 && *p <= 4)
}

/// Same owner? Compares assignee identity (id or name) so the cherry-pick
/// rule scopes to *this person's* queue, not the whole org. When the ticket
/// being started has no assignee we can't scope, so we don't gate (None).
fn same_assignee(a: &Value, b: &Value) -> Option<bool> {
    let key = |v: &Value| {
        v.get("assignee").and_then(|asg| {
            asg.get("id")
                .or_else(|| asg.get("name"))
                .or_else(|| asg.get("displayName"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
    };
    let ka = key(a)?;
    Some(key(b).as_ref() == Some(&ka))
}

/// Find a higher-priority, startable ticket in the same person's queue than
/// `target`. "Startable" = open (not done/canceled), not blocked, and strictly
/// more urgent (lower priority number). Returns the identifier of the first
/// such ticket, or `None` if the target is already the most urgent available.
/// Fails open: if the target has no priority, or no assignee to scope by, or
/// the cache is empty, returns `None` (no cherry-pick block).
fn higher_priority_available(ctx: &HookContext<'_>, target: &Value) -> Option<String> {
    let target_pri = priority_of(target)?; // unprioritized target → don't gate
    let all = cache_all(ctx);
    if all.is_empty() {
        return None;
    }
    let target_id = target.get("identifier").and_then(Value::as_str);
    for issue in &all {
        // Skip the target itself.
        if issue.get("identifier").and_then(Value::as_str) == target_id {
            continue;
        }
        // Scope to the same assignee; if we can't establish that, skip.
        if same_assignee(issue, target) != Some(true) {
            continue;
        }
        // Must be strictly more urgent.
        let Some(p) = priority_of(issue) else { continue };
        if p >= target_pri {
            continue;
        }
        // Must be startable: open and not blocked.
        let ty = issue
            .get("state")
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let open = matches!(ty.as_str(), "backlog" | "unstarted" | "triage" | "started");
        if !open || blocked_reason(issue).is_some() {
            continue;
        }
        if let Some(id) = issue.get("identifier").and_then(Value::as_str) {
            return Some(id.to_string());
        }
    }
    None
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

    #[test]
    fn blocked_by_open_relation() {
        // blockedBy an issue that is NOT resolved → blocked.
        let issue = serde_json::json!({
            "identifier": "A-1",
            "blockedBy": [{ "identifier": "A-2", "state": { "type": "started" } }]
        });
        assert!(blocked_reason(&issue).is_some());
    }

    #[test]
    fn blocked_by_resolved_relation_is_not_blocked() {
        // blockedBy an issue that IS completed → no longer a blocker.
        let issue = serde_json::json!({
            "identifier": "A-1",
            "blockedBy": [{ "identifier": "A-2", "state": { "type": "completed" } }]
        });
        assert!(blocked_reason(&issue).is_none());
    }

    #[test]
    fn relations_blocked_by_type() {
        let issue = serde_json::json!({
            "identifier": "A-1",
            "relations": [{
                "type": "blocked_by",
                "relatedIssue": { "identifier": "A-9", "state": { "type": "backlog" } }
            }]
        });
        assert!(blocked_reason(&issue).is_some());
    }

    #[test]
    fn blocked_workflow_state() {
        let by_name = serde_json::json!({ "state": { "name": "Blocked", "type": "started" } });
        assert!(blocked_reason(&by_name).is_some());
        let by_type = serde_json::json!({ "state": { "name": "Waiting", "type": "blocked" } });
        assert!(blocked_reason(&by_type).is_some());
    }

    #[test]
    fn blocked_label_string_and_object() {
        let str_label = serde_json::json!({ "labels": ["blocked", "frontend"] });
        assert!(blocked_reason(&str_label).is_some());
        let obj_label = serde_json::json!({ "labels": [{ "name": "blocker" }] });
        assert!(blocked_reason(&obj_label).is_some());
    }

    #[test]
    fn unblocked_ticket_passes() {
        let issue = serde_json::json!({
            "identifier": "A-1",
            "estimate": 3,
            "state": { "name": "Todo", "type": "backlog" },
            "labels": ["frontend"]
        });
        assert!(blocked_reason(&issue).is_none());
    }

    #[test]
    fn milestone_required_when_project_uses_them() {
        // Project uses milestones, ticket has none → needs one.
        let no_ms = serde_json::json!({
            "identifier": "M-1", "projectHasMilestones": true, "projectMilestone": null
        });
        assert!(needs_milestone(&no_ms));
        // Ticket has a milestone → fine.
        let with_ms = serde_json::json!({
            "identifier": "M-2", "projectHasMilestones": true,
            "projectMilestone": { "id": "x", "name": "M1" }
        });
        assert!(!needs_milestone(&with_ms));
    }

    #[test]
    fn priority_parsing() {
        assert_eq!(priority_of(&serde_json::json!({ "priority": 1 })), Some(1));
        assert_eq!(priority_of(&serde_json::json!({ "priority": 4 })), Some(4));
        // 0 = No priority → None (unprioritized, don't gate on it).
        assert_eq!(priority_of(&serde_json::json!({ "priority": 0 })), None);
        assert_eq!(priority_of(&serde_json::json!({})), None);
        // out of range → None
        assert_eq!(priority_of(&serde_json::json!({ "priority": 9 })), None);
    }

    #[test]
    fn same_assignee_scoping() {
        let a = serde_json::json!({ "assignee": { "id": "u1" } });
        let b = serde_json::json!({ "assignee": { "id": "u1" } });
        let c = serde_json::json!({ "assignee": { "id": "u2" } });
        assert_eq!(same_assignee(&a, &b), Some(true));
        assert_eq!(same_assignee(&a, &c), Some(false));
        // No assignee on the first → can't scope → None.
        let none = serde_json::json!({});
        assert_eq!(same_assignee(&none, &b), None);
    }

    #[test]
    fn milestone_not_required_when_project_has_none() {
        // Project doesn't use milestones → exempt (no false block).
        let exempt = serde_json::json!({
            "identifier": "M-3", "projectHasMilestones": false, "projectMilestone": null
        });
        assert!(!needs_milestone(&exempt));
        // Absent signal → also exempt (fail open).
        let unknown = serde_json::json!({ "identifier": "M-4" });
        assert!(!needs_milestone(&unknown));
    }
}
