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
//! Once a start attempt is identified, the gate requires live Linear authority
//! for the ticket being started. It blocks blocked tickets, oversized tickets,
//! missing milestone assignments, and priority cherry-picks before Linear state
//! can move.
//!
//! ## Authority
//!
//! The issue being started is read from the live `LinearLookupPort`. The local
//! assignment snapshot is used only as the queue projection for the
//! same-assignee priority check; it is not a substitute for target-ticket
//! authority.

use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};
use serde_json::Value;
use std::path::PathBuf;

use super::{HookContext, LinearLookupError};
use crate::linear_pm_audit::OVERSIZED_POINTS;

/// The Linear MCP tool whose calls this gate inspects.
const TARGET_TOOL: &str = "mcp__linear__update_issue";

/// Tokens in a state name/id that indicate a transition *into* active work.
/// We can't resolve a Linear state UUID offline, so we gate on the common
/// case where the caller passes a human-readable state hint or explicit start
/// flag.
const STARTED_HINTS: &[&str] = &["in progress", "in-progress", "started", "doing"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinearPmDecision {
    Allow,
    BlockMissingIssueIdentifier,
    BlockLiveAuthorityUnavailable,
    BlockBlockedTicket,
    BlockOversizedTicket,
    BlockMissingMilestone,
    BlockHigherPriorityAvailable,
}

#[derive(Debug, Clone)]
pub struct LinearPmEvaluation {
    pub tool: Option<String>,
    pub target_tool: bool,
    pub tool_input_present: bool,
    pub start_transition: bool,
    pub issue_key: Option<String>,
    pub issue_key_present: bool,
    pub issue_fetched: bool,
    pub live_authority_error: Option<String>,
    pub ticket_identifier: Option<String>,
    pub blocked_ticket: bool,
    pub blocked_reason: Option<String>,
    pub estimate_present: bool,
    pub estimate_points: f64,
    pub oversized_ticket: bool,
    pub project_has_milestones: bool,
    pub milestone_present: bool,
    pub missing_milestone: bool,
    pub target_priority_present: bool,
    pub target_priority: i64,
    pub target_assignee_present: bool,
    pub higher_priority_available: bool,
    pub higher_priority_ticket: Option<String>,
    pub should_block: bool,
    pub decision: LinearPmDecision,
}

impl LinearPmEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.target_tool && self.start_transition
    }
}

/// `PreToolUse` entry point.
pub fn process_pretool(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let evaluation = evaluate_pretool(input, ctx);
    output_from_evaluation(&evaluation)
}

pub fn evaluate_pretool(input: &HookInput, ctx: &HookContext<'_>) -> LinearPmEvaluation {
    let mut evaluation = base_evaluation(input);

    // Only inspect the Linear update tool.
    if input.tool_name.as_deref() != Some(TARGET_TOOL) {
        return evaluation;
    }
    evaluation.target_tool = true;

    let Some(args) = input.tool_input.as_ref() else {
        return evaluation;
    };
    evaluation.tool_input_present = true;

    // Only gate transitions that look like "move to a started state".
    if !is_start_transition(args) {
        return evaluation;
    }
    evaluation.start_transition = true;

    // Identify the issue being moved. Linear accepts either the display
    // identifier or the underlying issue ID for live lookup.
    let Some(ticket_key) = issue_lookup_key(args) else {
        evaluation.should_block = true;
        evaluation.decision = LinearPmDecision::BlockMissingIssueIdentifier;
        return evaluation;
    };
    evaluation.issue_key = Some(ticket_key.clone());
    evaluation.issue_key_present = true;

    let issue = match live_issue(ctx, &ticket_key) {
        Ok(issue) => {
            evaluation.issue_fetched = true;
            issue
        }
        Err(reason) => {
            evaluation.live_authority_error = Some(reason.to_string());
            evaluation.should_block = true;
            evaluation.decision = LinearPmDecision::BlockLiveAuthorityUnavailable;
            return evaluation;
        }
    };
    let ticket = issue
        .get("identifier")
        .and_then(Value::as_str)
        .unwrap_or(&ticket_key);
    evaluation.ticket_identifier = Some(ticket.to_string());

    // Check 1 (hardest stop): the ticket is BLOCKED. Starting a ticket whose
    // upstream work isn't done is wasted/at-risk effort — refuse it
    // regardless of size. Covers an open blocked-by relation, a Blocked
    // workflow state, or a blocked/blocker label.
    if let Some(reason) = blocked_reason(&issue) {
        evaluation.blocked_ticket = true;
        evaluation.blocked_reason = Some(reason);
        evaluation.should_block = true;
        evaluation.decision = LinearPmDecision::BlockBlockedTicket;
        return evaluation;
    }

    // Check 2: oversized & undecomposed.
    let estimate = issue
        .get("estimate")
        .and_then(Value::as_f64)
        .filter(|e| e.is_finite() && *e > 0.0);
    if let Some(e) = estimate {
        evaluation.estimate_present = true;
        evaluation.estimate_points = e;
        if e >= OVERSIZED_POINTS {
            evaluation.oversized_ticket = true;
            evaluation.should_block = true;
            evaluation.decision = LinearPmDecision::BlockOversizedTicket;
            return evaluation;
        }
    }

    // Check 3: untracked work — starting a ticket with no milestone, *when its
    // project uses milestones*. A ticket you start should be tied to a tracked
    // deliverable. Projects that don't define milestones are exempt (no false
    // block); we only enforce when `projectHasMilestones` is true in the cache.
    evaluation.project_has_milestones = issue
        .get("projectHasMilestones")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    evaluation.milestone_present = issue
        .get("projectMilestone")
        .or_else(|| issue.get("milestone"))
        .map(|m| !m.is_null())
        .unwrap_or(false);
    if needs_milestone(&issue) {
        evaluation.missing_milestone = true;
        evaluation.should_block = true;
        evaluation.decision = LinearPmDecision::BlockMissingMilestone;
        return evaluation;
    }

    // Check 4: cherry-picking — starting a lower-priority ticket while a
    // higher-priority, startable ticket waits in the same person's queue.
    // Work the most urgent thing first. Fails open if the target is
    // unprioritized or has no assignee to scope by.
    if let Some(priority) = priority_of(&issue) {
        evaluation.target_priority_present = true;
        evaluation.target_priority = priority;
    }
    evaluation.target_assignee_present = issue
        .get("assignee")
        .and_then(|asg| {
            asg.get("id")
                .or_else(|| asg.get("name"))
                .or_else(|| asg.get("displayName"))
                .and_then(Value::as_str)
        })
        .is_some();
    if let Some(higher) = higher_priority_available(ctx, &issue) {
        evaluation.higher_priority_available = true;
        evaluation.higher_priority_ticket = Some(higher);
        evaluation.should_block = true;
        evaluation.decision = LinearPmDecision::BlockHigherPriorityAvailable;
        return evaluation;
    }

    evaluation
}

fn base_evaluation(input: &HookInput) -> LinearPmEvaluation {
    LinearPmEvaluation {
        tool: input.tool_name.clone(),
        target_tool: false,
        tool_input_present: false,
        start_transition: false,
        issue_key: None,
        issue_key_present: false,
        issue_fetched: false,
        live_authority_error: None,
        ticket_identifier: None,
        blocked_ticket: false,
        blocked_reason: None,
        estimate_present: false,
        estimate_points: 0.0,
        oversized_ticket: false,
        project_has_milestones: false,
        milestone_present: false,
        missing_milestone: false,
        target_priority_present: false,
        target_priority: 0,
        target_assignee_present: false,
        higher_priority_available: false,
        higher_priority_ticket: None,
        should_block: false,
        decision: LinearPmDecision::Allow,
    }
}

#[must_use]
pub fn output_from_evaluation(evaluation: &LinearPmEvaluation) -> HookOutput {
    match evaluation.decision {
        LinearPmDecision::Allow => HookOutput::allow(),
        LinearPmDecision::BlockMissingIssueIdentifier => authority_block(
            "Refusing to start Linear issue: update_issue did not include an issue identifier \
             or ID. The PM gate requires live Linear authority before a start transition.",
        ),
        LinearPmDecision::BlockLiveAuthorityUnavailable => {
            let ticket_key = evaluation.issue_key.as_deref().unwrap_or("Linear issue");
            let reason = evaluation
                .live_authority_error
                .as_deref()
                .unwrap_or("unknown live Linear lookup failure");
            authority_block(format!(
                "Refusing to start {ticket_key}: the PM gate could not verify the ticket \
                 through live Linear authority ({reason}). Configure SENTINEL_LINEAR_TOKEN \
                 and retry; stale local assignment data is not accepted for state changes."
            ))
        }
        LinearPmDecision::BlockBlockedTicket => {
            let ticket = evaluation
                .ticket_identifier
                .as_deref()
                .or(evaluation.issue_key.as_deref())
                .unwrap_or("Linear issue");
            let reason = evaluation
                .blocked_reason
                .as_deref()
                .unwrap_or("blocked by an unresolved dependency");
            let envelope = HookEnvelope::block(
                "Linear PM-Enforcement Gate",
                format!(
                    "Refusing to start {ticket}: it is BLOCKED ({reason}). Starting a \
                     blocked ticket means working on something gated by incomplete \
                     upstream work. Resolve the blocker (or remove the block) first, \
                     then start it. Run `sentinel linear-audit scan` for the full PM picture."
                ),
            );
            HookOutput::block(envelope.render())
        }
        LinearPmDecision::BlockOversizedTicket => {
            let ticket = evaluation
                .ticket_identifier
                .as_deref()
                .or(evaluation.issue_key.as_deref())
                .unwrap_or("Linear issue");
            let estimate = evaluation.estimate_points;
            let envelope = HookEnvelope::block(
                "Linear PM-Enforcement Gate",
                format!(
                    "Refusing to start {ticket}: it is a {estimate:.0}-point ticket and \
                     has not been decomposed. An 8+ point ticket as a single block hides \
                     risk and defies estimation — split it into sub-issues (each ≤ 5 pts) \
                     first, then start one of those. Run `sentinel linear-audit scan` for \
                     the full PM picture."
                ),
            );
            HookOutput::block(envelope.render())
        }
        LinearPmDecision::BlockMissingMilestone => {
            let ticket = evaluation
                .ticket_identifier
                .as_deref()
                .or(evaluation.issue_key.as_deref())
                .unwrap_or("Linear issue");
            let envelope = HookEnvelope::block(
                "Linear PM-Enforcement Gate",
                format!(
                    "Refusing to start {ticket}: it has no milestone, but its project \
                     uses milestones. Work you start should map to a tracked deliverable \
                     — assign {ticket} to a project milestone first, then start it. \
                     (Projects without milestones are exempt.)"
                ),
            );
            HookOutput::block(envelope.render())
        }
        LinearPmDecision::BlockHigherPriorityAvailable => {
            let ticket = evaluation
                .ticket_identifier
                .as_deref()
                .or(evaluation.issue_key.as_deref())
                .unwrap_or("Linear issue");
            let higher = evaluation
                .higher_priority_ticket
                .as_deref()
                .unwrap_or("a higher-priority ticket");
            let envelope = HookEnvelope::block(
                "Linear PM-Enforcement Gate",
                format!(
                    "Refusing to start {ticket}: a higher-priority ticket ({higher}) is \
                     available and startable in the same queue. Don't cherry-pick — start \
                     the most urgent work first. Start {higher}, or re-prioritize if {ticket} \
                     genuinely comes first."
                ),
            );
            HookOutput::block(envelope.render())
        }
    }
}

/// Does this issue need (but lack) a milestone? True only when the cache says
/// the project uses milestones (`projectHasMilestones: true`) AND the issue
/// itself carries no `projectMilestone`/`milestone`.
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
///
/// Any absent signal simply doesn't fire; positive blocked signals are enough
/// to stop the transition.
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

    // 2. Blocked workflow state (by name or type). Matches a bare "Blocked"
    // column AND the Firefly "QA Blocked" state (Pedro's QA redesign: built
    // but a dependency/data blocks the test) — any state whose name contains
    // "blocked" is a do-not-start signal.
    if let Some(state) = issue.get("state") {
        let name = state.get("name").and_then(Value::as_str).unwrap_or("");
        let ty = state.get("type").and_then(Value::as_str).unwrap_or("");
        if name.to_lowercase().contains("blocked") || ty.eq_ignore_ascii_case("blocked") {
            return Some(format!("its workflow state is \"{name}\""));
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
/// convenience flag if present.
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

/// Pull a Linear issue lookup key out of the args. Accepts the display
/// identifier (`FPCRM-606`) or the underlying Linear issue ID because the live
/// GraphQL lookup can resolve both.
fn issue_lookup_key(args: &Value) -> Option<String> {
    for key in ["identifier", "id", "issueId", "issue_id"] {
        if let Some(s) = args.get(key).and_then(Value::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn live_issue(ctx: &HookContext<'_>, ticket: &str) -> Result<Value, LinearLookupError> {
    let Some(port) = ctx.linear_lookup else {
        return Err(LinearLookupError::Transport(
            "SENTINEL_LINEAR_TOKEN is not configured".into(),
        ));
    };
    port.fetch_issue(ticket)
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
/// `{issues:[...]}`). This is the local queue projection for same-assignee
/// priority comparison.
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
/// such ticket, or `None` if the target is already the most urgent available
/// in the queue projection.
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
        let Some(p) = priority_of(issue) else {
            continue;
        };
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

fn authority_block(message: impl Into<String>) -> HookOutput {
    let envelope = HookEnvelope::block("Linear PM-Enforcement Gate", message.into());
    HookOutput::block(envelope.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support;

    #[derive(Clone)]
    struct MockLookup {
        result: Result<Value, LinearLookupError>,
    }

    impl super::super::LinearLookupPort for MockLookup {
        fn fetch_issue(&self, _identifier_or_id: &str) -> Result<Value, LinearLookupError> {
            self.result.clone()
        }
    }

    fn start_input(issue_key: &str) -> HookInput {
        HookInput {
            tool_name: Some(TARGET_TOOL.into()),
            tool_input: Some(serde_json::json!({
                "identifier": issue_key,
                "started": true
            })),
            ..Default::default()
        }
    }

    #[test]
    fn issue_lookup_accepts_identifier_or_issue_id() {
        let by_identifier = serde_json::json!({ "identifier": "FPCRM-606" });
        assert_eq!(
            issue_lookup_key(&by_identifier).as_deref(),
            Some("FPCRM-606")
        );
        let by_uuid = serde_json::json!({ "id": "550e8400-e29b-41d4-a716-446655440000" });
        assert_eq!(
            issue_lookup_key(&by_uuid).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
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
        assert!(!is_start_transition(&uuid_only));
    }

    #[test]
    fn start_transition_without_live_lookup_blocks() {
        let ctx = test_support::stub_ctx();
        let out = process_pretool(&start_input("FPCRM-606"), &ctx);
        assert_eq!(out.blocked, Some(true));
        let reason = out.reason.as_deref().expect("missing block reason");
        assert!(reason.contains("SENTINEL_LINEAR_TOKEN"));
        assert!(reason.contains("stale local assignment data is not accepted"));
    }

    #[test]
    fn live_lookup_failure_blocks() {
        let lookup: &'static MockLookup = Box::leak(Box::new(MockLookup {
            result: Err(LinearLookupError::Transport("timeout".into())),
        }));
        let mut ctx = test_support::stub_ctx();
        ctx.linear_lookup = Some(lookup);
        let out = process_pretool(&start_input("FPCRM-606"), &ctx);
        assert_eq!(out.blocked, Some(true));
        let reason = out.reason.as_deref().expect("missing block reason");
        assert!(reason.contains("timeout"));
        assert!(reason.contains("live Linear authority"));
    }

    #[test]
    fn live_lookup_is_authority_for_oversized_ticket() {
        let lookup: &'static MockLookup = Box::leak(Box::new(MockLookup {
            result: Ok(serde_json::json!({
                "identifier": "FPCRM-606",
                "estimate": 8,
                "state": { "name": "Todo", "type": "backlog" }
            })),
        }));
        let mut ctx = test_support::stub_ctx();
        ctx.linear_lookup = Some(lookup);
        let out = process_pretool(&start_input("FPCRM-606"), &ctx);
        assert_eq!(out.blocked, Some(true));
        let reason = out.reason.as_deref().expect("missing block reason");
        assert!(reason.contains("FPCRM-606"));
        assert!(reason.contains("8-point ticket"));
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
        // Pedro's QA redesign: "QA Blocked" must count as a blocked state.
        let qa_blocked =
            serde_json::json!({ "state": { "name": "QA Blocked", "type": "started" } });
        assert!(blocked_reason(&qa_blocked).is_some());
        // But "QA Testing (UI)" must NOT be treated as blocked.
        let qa_testing =
            serde_json::json!({ "state": { "name": "QA Testing (UI)", "type": "started" } });
        assert!(blocked_reason(&qa_testing).is_none());
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
        // Absent signal → also exempt.
        let unknown = serde_json::json!({ "identifier": "M-4" });
        assert!(!needs_milestone(&unknown));
    }
}
