//! Ticket Quality Gate — PreToolUse advisory hook
//!
//! Surfaces the Linear ticket-quality rubric + score gate at the exact moment a
//! ticket is created or groomed, so the discipline in
//! `~/.claude/skills/linear/best-practices/ticket-quality.md` is never silently
//! skipped. Scoped to the Linear create/update MCP tools ONLY.
//!
//! ## ADVISORY, never a deny — by design
//! This hook NEVER blocks. It injects `additionalContext` reminding the agent to
//! (a) resolve the team's estimation type, (b) set every gate-required field
//! (Type+Area labels, estimate, priority, ≥3 testable AC), and (c) re-fetch the
//! saved ticket and score it against the ≥95 gate before reporting done.
//!
//! The advisory-only contract is deliberate and load-bearing: a *blocking*
//! quality gate keyed on a Linear tool is exactly the shape that produced the
//! whole-session phase-gate deadlock (a workflow that traps a tool with no
//! satisfiable escape). A reminder enforces quality without ever trapping the
//! session — the agent still makes the call; it just gets the rubric in context.
//!
//! ## Scope
//! Fires only when `tool_name` is `mcp__linear__create_issue` or
//! `mcp__linear__update_issue`. Every other tool — including every other Linear
//! MCP tool (search, get_issue, list_labels, …) — passes through untouched, so
//! this hook can never gate read/discovery work or anything outside ticket
//! creation/grooming.
//!
//! ## Fail-open
//! No session, missing fields, panic — always returns [`HookOutput::allow`].
//! Reminder-only; must never block a tool call.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// The Linear MCP tools that create or mutate a ticket's fields. Only these
/// trigger the advisory — keeping the hook tightly scoped so it can never gate
/// discovery/read tools or non-Linear work.
const TICKET_WRITE_TOOLS: &[&str] = &[
    "mcp__linear__create_issue",
    "mcp__linear__update_issue",
];

/// Is this tool a Linear ticket create/update call?
fn is_ticket_write_tool(tool_name: &str) -> bool {
    TICKET_WRITE_TOOLS.contains(&tool_name)
}

/// The advisory injected before a ticket create/update. Kept compact — it points
/// at the authoritative rubric file and lists the binary gate so the agent has
/// the checklist in-context without re-reading the whole skill.
fn advisory_for(tool_name: &str) -> String {
    let action = if tool_name.ends_with("create_issue") {
        "creating"
    } else {
        "updating"
    };
    format!(
        "[Ticket Quality Gate] You are {action} a Linear ticket. Hold it to the score gate \
         (~/.claude/skills/linear/best-practices/ticket-quality.md): \n\
         1. Resolve the team's estimation type FIRST (get_team → fibonacci/tShirt/notUsed; never \
         hardcode Fibonacci) and the team's REAL label taxonomy (list_labels — Area may be \
         domain-based, not Frontend/Backend).\n\
         2. Set every field IN THE CALL: Type + Area labels, estimate (team's type), priority 1–4 \
         (never 0), and a description with Context + Scope + ≥3 testable Acceptance-Criteria \
         checkboxes.\n\
         3. After the write, RE-FETCH the ticket (get_issue by the returned id) — MCP create calls \
         silently drop labels/estimate/priority. Confirm they persisted; re-apply any that didn't.\n\
         4. SCORE the re-fetched ticket on the 0–100 rubric and check the binary gates. Repair the \
         lowest dimensions + any failed gate, then re-fetch and re-score until score ≥ 95 AND all \
         gates pass. Report 'ID — quality N/100, gate: PASS'. Advisory only — sentinel does not \
         block this call."
    )
}

/// Process a `PreToolUse` event. Advisory-only: injects the rubric reminder for
/// Linear ticket create/update tools, allows everything else.
pub fn process(input: &HookInput) -> HookOutput {
    let tool_name = match input.tool_name.as_deref() {
        Some(name) => name,
        None => return HookOutput::allow(),
    };

    if !is_ticket_write_tool(tool_name) {
        return HookOutput::allow();
    }

    HookOutput::inject_context(HookEvent::PreToolUse, advisory_for(tool_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_for(tool: Option<&str>) -> HookInput {
        HookInput {
            tool_name: tool.map(str::to_string),
            ..Default::default()
        }
    }

    fn injected(out: &HookOutput) -> Option<String> {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
    }

    #[test]
    fn create_issue_injects_advisory() {
        let out = process(&input_for(Some("mcp__linear__create_issue")));
        let msg = injected(&out).expect("create_issue must inject the rubric advisory");
        assert!(msg.contains("Ticket Quality Gate"), "{msg}");
        assert!(msg.contains("estimation type"), "{msg}");
        assert!(msg.contains("RE-FETCH"), "{msg}");
        assert!(msg.contains("≥ 95") || msg.contains(">= 95"), "{msg}");
        // ADVISORY: never blocks.
        assert!(out.blocked.is_none(), "advisory hook must never block");
    }

    #[test]
    fn update_issue_injects_advisory() {
        let out = process(&input_for(Some("mcp__linear__update_issue")));
        let msg = injected(&out).expect("update_issue must inject the advisory");
        assert!(msg.contains("updating"), "{msg}");
        assert!(out.blocked.is_none());
    }

    #[test]
    fn other_linear_tools_pass_through() {
        // Read/discovery Linear tools must NOT be gated — only create/update.
        for tool in [
            "mcp__linear__search",
            "mcp__linear__get_issue",
            "mcp__linear__list_labels",
            "mcp__linear__create_comment",
            "mcp__linear__add_issue_label",
        ] {
            let out = process(&input_for(Some(tool)));
            assert!(
                injected(&out).is_none(),
                "{tool} must pass through (only create/update issue trigger the advisory)"
            );
            assert!(out.blocked.is_none());
        }
    }

    #[test]
    fn non_linear_tools_pass_through() {
        for tool in ["Bash", "Write", "Edit", "mcp__github__create_issue"] {
            let out = process(&input_for(Some(tool)));
            assert!(injected(&out).is_none(), "{tool} must pass through");
            assert!(out.blocked.is_none());
        }
    }

    #[test]
    fn missing_tool_name_allows() {
        let out = process(&input_for(None));
        assert!(injected(&out).is_none());
        assert!(out.blocked.is_none());
    }

    #[test]
    fn never_blocks_invariant() {
        // Exhaustive: for every tool we care about, the hook is advisory-only.
        for tool in [
            "mcp__linear__create_issue",
            "mcp__linear__update_issue",
            "mcp__linear__search",
            "Bash",
        ] {
            let out = process(&input_for(Some(tool)));
            assert!(
                out.blocked.is_none(),
                "ticket_quality_gate must NEVER block ({tool})"
            );
        }
    }
}
