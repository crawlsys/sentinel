//! Ticket Quality Gate — `PreToolUse` ENFORCING hook
//!
//! Enforces the Linear "Definition of Ready" at the exact moment a ticket is
//! created or groomed, so a half-baked ticket can never enter the workflow.
//! This is Tier A of the "bad PM is bad software" factory: the in-session
//! pre-write gate.
//!
//! ## ENFORCING — denies, with a guided fix-path
//! When an agent calls `mcp__linear__create_issue` / `update_issue` WITHOUT the
//! required readiness fields, this hook returns [`HookOutput::deny`] (carrying
//! the unspoofable `[Sentinel-Authority]` prefix). The deny reason names the
//! exact missing fields AND the remediation: use the built-in `AskUserQuestion`
//! Q&A to collect them, then re-issue the call. So the block is never a wall —
//! it is block + guided remediation.
//!
//! ## Why this is NOT the old phase-gate deadlock
//! The advisory-only contract used to be load-bearing because a *blocking* gate
//! keyed on a tool with no satisfiable escape traps the session. This gate has
//! a satisfiable escape BY CONSTRUCTION:
//!   * it only fires on a Linear ticket WRITE (create/update/claim) — never on
//!     reads, never on non-Linear tools, never when you are not working a ticket;
//!   * the fix is always available: add the missing fields to the very same call
//!     (the agent has them, or collects them via Q&A) and retry.
//!
//! There is no state the agent can be in where it is blocked with no way forward.
//!
//! ## The Definition of Ready (the bar)
//! A ticket write must carry: an `estimate`, a `priority` in `1..=4` (never 0),
//! at least one Type + one Area label (via `labelIds`), and a `description`
//! with real substance (Context + acceptance criteria). On `create` all are
//! required; on `update` we only enforce a field the call is *setting to an
//! empty/zero value* (you can update a ticket's title without re-supplying every
//! field — but you cannot null out its estimate/priority).
//!
//! ## Scope — the humane invariant
//! Fires ONLY for `mcp__linear__create_issue` / `update_issue`. Every other tool
//! — all non-Linear tools, and every read/discovery Linear tool (`search`,
//! `get_issue`, `list_labels`, `add_issue_label`, `create_comment`, …) — passes
//! through untouched. If you are not writing a Linear ticket, this gate does not exist.
//!
//! ## Fail-visible
//! Non-ticket tools still pass through untouched. A ticket write with absent or
//! malformed input is blocked because Sentinel cannot prove the write satisfies
//! the Definition of Ready.

use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest as _, Sha256};

/// The Linear MCP tools that create or mutate a ticket's fields. Only these are
/// gated — keeping the hook tightly scoped so it can never gate discovery/read
/// tools or non-Linear work.
const TICKET_WRITE_TOOLS: &[&str] = &["mcp__linear__create_issue", "mcp__linear__update_issue"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketQualityDecision {
    Allow,
    DenyMalformedInput,
    DenyMissingFields,
}

#[derive(Debug, Clone)]
pub struct TicketQualityEvaluation {
    pub tool: Option<String>,
    pub ticket_write_tool: bool,
    pub create_issue: bool,
    pub update_issue: bool,
    pub tool_input_present: bool,
    pub tool_input_object: bool,
    pub tool_input_sha256: Option<String>,
    pub missing: Vec<Missing>,
    pub malformed_reason: Option<&'static str>,
    pub missing_estimate: bool,
    pub missing_priority: bool,
    pub missing_label_ids: bool,
    pub missing_description: bool,
    pub should_deny: bool,
    pub decision: TicketQualityDecision,
}

impl TicketQualityEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.ticket_write_tool
    }
}

/// Is this tool a Linear ticket create/update call?
fn is_ticket_write_tool(tool_name: &str) -> bool {
    TICKET_WRITE_TOOLS.contains(&tool_name)
}

/// A single readiness dimension the tool input failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Missing {
    pub field: &'static str,
    pub why: &'static str,
}

/// Inspect the `tool_input` JSON for a create/update call and return every
/// readiness field that is absent (create) or actively cleared (update).
///
/// `is_create` toggles strictness: on create, a *missing* field is a violation;
/// on update, only a field present-but-empty/zero is a violation (so you can
/// patch a title without re-supplying the whole ticket).
fn missing_fields(input: &serde_json::Value, is_create: bool) -> Vec<Missing> {
    let mut out = Vec::new();

    // estimate: number > 0
    match input.get("estimate") {
        Some(v) if v.is_null() => out.push(Missing {
            field: "estimate",
            why: "estimate is null",
        }),
        Some(v) if v.as_f64().is_some_and(|n| n <= 0.0) => {
            out.push(Missing {
                field: "estimate",
                why: "estimate must be > 0",
            });
        }
        Some(_) => {}
        None if is_create => out.push(Missing {
            field: "estimate",
            why: "no estimate set",
        }),
        None => {}
    }

    // priority: integer 1..=4 (Linear: 0 = No priority, which we forbid)
    match input.get("priority") {
        Some(v) if v.is_null() => out.push(Missing {
            field: "priority",
            why: "priority is null",
        }),
        Some(v) => match v.as_i64() {
            Some(p) if !(1..=4).contains(&p) => {
                out.push(Missing {
                    field: "priority",
                    why: "priority must be 1-4 (not 0/No-priority)",
                });
            }
            Some(_) => {}
            None => out.push(Missing {
                field: "priority",
                why: "priority must be an integer 1-4",
            }),
        },
        None if is_create => out.push(Missing {
            field: "priority",
            why: "no priority set",
        }),
        None => {}
    }

    // labels: at least one labelId (Type + Area enforced at the bar; we require
    // the call to carry labelIds — the agent maps Type/Area to ids). On update,
    // only flag an explicitly-emptied labelIds array.
    match input.get("labelIds") {
        Some(v) if v.as_array().is_some_and(std::vec::Vec::is_empty) => {
            out.push(Missing {
                field: "labelIds",
                why: "labelIds is empty - need Type + Area",
            });
        }
        Some(v) if v.is_null() => out.push(Missing {
            field: "labelIds",
            why: "labelIds is null",
        }),
        Some(_) => {}
        None if is_create => out.push(Missing {
            field: "labelIds",
            why: "no labels - need Type + Area",
        }),
        None => {}
    }

    // description: substantive (Context + acceptance criteria). Heuristic: a
    // ready ticket's description is non-trivial. We require >= 80 chars on create.
    match input.get("description").and_then(serde_json::Value::as_str) {
        Some(d) if d.trim().chars().count() < 80 => {
            out.push(Missing {
                field: "description",
                why: "description too thin - need Context + >=3 acceptance criteria",
            });
        }
        Some(_) => {}
        None if is_create => out.push(Missing {
            field: "description",
            why: "no description - need Context + >=3 acceptance criteria",
        }),
        None => {}
    }

    out
}

/// Build the `[Sentinel-Authority]`-tagged deny reason: names the violations and
/// the guided fix-path (the Q&A remediation), so the block always has a way out.
fn deny_reason(action: &str, missing: &[Missing]) -> String {
    let mut s = String::with_capacity(512);
    s.push_str("Ticket Quality Gate - this Linear ticket is NOT dev-ready, so ");
    s.push_str(action);
    s.push_str(" is blocked (bad PM is bad software). Missing/invalid:\n");
    for m in missing {
        s.push_str("  - ");
        s.push_str(m.field);
        s.push_str(" - ");
        s.push_str(m.why);
        s.push('\n');
    }
    s.push_str(
        "\nFIX-PATH (the gate helps you, it doesn't just wall you off):\n\
         1. If you don't already know a value (which estimate? which Area label? the acceptance criteria?), \
         ASK the user with the AskUserQuestion tool - one question per missing field - instead of guessing.\n\
         2. Resolve the team's estimation type + real label taxonomy first (get_team, list_labels).\n\
         3. Re-issue this exact call WITH: estimate (>0), priority 1-4, labelIds (Type + Area), and a \
         description containing Context + >=3 testable acceptance-criteria checkboxes.\n\
         This gate ONLY applies because you are writing a Linear ticket - it never touches non-Linear work.",
    );
    s
}

fn malformed_input_reason(action: &str, why: &str) -> String {
    format!(
        "Ticket Quality Gate - this Linear ticket write cannot be proven dev-ready, so {action} \
         is blocked. Reason: {why}.\n\n\
         FIX-PATH: re-issue the Linear ticket write with a JSON object carrying the readiness \
         fields Sentinel can inspect: estimate (>0), priority 1-4, labelIds (Type + Area), \
         and a description containing Context + >=3 testable acceptance-criteria checkboxes."
    )
}

/// Process a `PreToolUse` event. ENFORCING for Linear ticket create/update:
/// denies (with the Q&A fix-path) when the readiness bar is unmet; allows
/// everything else.
#[must_use]
pub fn process(input: &HookInput) -> HookOutput {
    let evaluation = evaluate(input);
    output_from_evaluation(&evaluation)
}

#[must_use]
pub fn evaluate(input: &HookInput) -> TicketQualityEvaluation {
    let tool = input.tool_name.clone();
    let Some(tool_name) = input.tool_name.as_deref() else {
        return base_evaluation(tool);
    };

    if !is_ticket_write_tool(tool_name) {
        return base_evaluation(tool);
    }

    let create_issue = tool_name.ends_with("create_issue");
    let update_issue = tool_name.ends_with("update_issue");
    let Some(tool_input) = input.tool_input.as_ref() else {
        return TicketQualityEvaluation {
            tool,
            ticket_write_tool: true,
            create_issue,
            update_issue,
            tool_input_present: false,
            tool_input_object: false,
            tool_input_sha256: None,
            missing: Vec::new(),
            malformed_reason: Some("missing tool_input"),
            missing_estimate: false,
            missing_priority: false,
            missing_label_ids: false,
            missing_description: false,
            should_deny: true,
            decision: TicketQualityDecision::DenyMalformedInput,
        };
    };
    if !tool_input.is_object() {
        return TicketQualityEvaluation {
            tool,
            ticket_write_tool: true,
            create_issue,
            update_issue,
            tool_input_present: true,
            tool_input_object: false,
            tool_input_sha256: Some(json_sha256(tool_input)),
            missing: Vec::new(),
            malformed_reason: Some("tool_input must be a JSON object"),
            missing_estimate: false,
            missing_priority: false,
            missing_label_ids: false,
            missing_description: false,
            should_deny: true,
            decision: TicketQualityDecision::DenyMalformedInput,
        };
    }

    let missing = missing_fields(tool_input, create_issue);
    let missing_estimate = missing.iter().any(|m| m.field == "estimate");
    let missing_priority = missing.iter().any(|m| m.field == "priority");
    let missing_label_ids = missing.iter().any(|m| m.field == "labelIds");
    let missing_description = missing.iter().any(|m| m.field == "description");
    let should_deny = !missing.is_empty();
    TicketQualityEvaluation {
        tool,
        ticket_write_tool: true,
        create_issue,
        update_issue,
        tool_input_present: true,
        tool_input_object: true,
        tool_input_sha256: Some(json_sha256(tool_input)),
        missing,
        malformed_reason: None,
        missing_estimate,
        missing_priority,
        missing_label_ids,
        missing_description,
        should_deny,
        decision: if should_deny {
            TicketQualityDecision::DenyMissingFields
        } else {
            TicketQualityDecision::Allow
        },
    }
}

fn base_evaluation(tool: Option<String>) -> TicketQualityEvaluation {
    TicketQualityEvaluation {
        tool,
        ticket_write_tool: false,
        create_issue: false,
        update_issue: false,
        tool_input_present: false,
        tool_input_object: false,
        tool_input_sha256: None,
        missing: Vec::new(),
        malformed_reason: None,
        missing_estimate: false,
        missing_priority: false,
        missing_label_ids: false,
        missing_description: false,
        should_deny: false,
        decision: TicketQualityDecision::Allow,
    }
}

fn json_sha256(value: &serde_json::Value) -> String {
    hex::encode(Sha256::digest(value.to_string().as_bytes()))
}

#[must_use]
pub fn output_from_evaluation(evaluation: &TicketQualityEvaluation) -> HookOutput {
    let action = if evaluation.create_issue {
        "creating it"
    } else {
        "this update"
    };
    match evaluation.decision {
        TicketQualityDecision::Allow => HookOutput::allow(),
        TicketQualityDecision::DenyMalformedInput => HookOutput::deny(malformed_input_reason(
            action,
            evaluation
                .malformed_reason
                .unwrap_or("tool_input cannot be inspected"),
        )),
        TicketQualityDecision::DenyMissingFields => {
            HookOutput::deny(deny_reason(action, &evaluation.missing))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input_for(tool: Option<&str>, ti: Option<serde_json::Value>) -> HookInput {
        HookInput {
            tool_name: tool.map(str::to_string),
            tool_input: ti,
            ..Default::default()
        }
    }

    fn is_denied(out: &HookOutput) -> bool {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision.as_ref())
            .is_some_and(|d| matches!(d, sentinel_domain::events::PermissionDecision::Deny))
    }

    fn deny_text(out: &HookOutput) -> Option<String> {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.permission_decision_reason.clone())
    }

    // A fully dev-ready create payload.
    fn ready_create() -> serde_json::Value {
        json!({
            "title": "Add inline editing to deal stage",
            "estimate": 3,
            "priority": 2,
            "labelIds": ["type-feature-id", "area-deals-id"],
            "description": "## Context\nDeal stage isn't editable inline.\n## Acceptance criteria\n- [ ] click-to-edit on the stage chip\n- [ ] persists via PATCH /deals/:id\n- [ ] optimistic update + rollback on error"
        })
    }

    #[test]
    fn ready_create_passes() {
        let out = process(&input_for(
            Some("mcp__linear__create_issue"),
            Some(ready_create()),
        ));
        assert!(!is_denied(&out), "a fully dev-ready create must pass");
    }

    #[test]
    fn create_missing_estimate_denied() {
        let mut p = ready_create();
        p.as_object_mut().unwrap().remove("estimate");
        let out = process(&input_for(Some("mcp__linear__create_issue"), Some(p)));
        assert!(is_denied(&out), "create without estimate must be denied");
        let r = deny_text(&out).unwrap();
        assert!(r.contains("estimate"), "{r}");
        assert!(
            r.contains("AskUserQuestion"),
            "deny must name the Q&A fix-path: {r}"
        );
        assert!(
            r.starts_with("[Sentinel-Authority]"),
            "must carry authority prefix: {r}"
        );
    }

    #[test]
    fn create_priority_zero_denied() {
        let mut p = ready_create();
        p["priority"] = json!(0);
        let out = process(&input_for(Some("mcp__linear__create_issue"), Some(p)));
        assert!(is_denied(&out));
        assert!(deny_text(&out).unwrap().contains("priority"));
    }

    #[test]
    fn create_no_labels_denied() {
        let mut p = ready_create();
        p.as_object_mut().unwrap().remove("labelIds");
        let out = process(&input_for(Some("mcp__linear__create_issue"), Some(p)));
        assert!(is_denied(&out));
        assert!(deny_text(&out).unwrap().contains("Type + Area"));
    }

    #[test]
    fn create_thin_description_denied() {
        let mut p = ready_create();
        p["description"] = json!("fix it");
        let out = process(&input_for(Some("mcp__linear__create_issue"), Some(p)));
        assert!(is_denied(&out));
        assert!(deny_text(&out).unwrap().contains("description"));
    }

    #[test]
    fn update_title_only_passes() {
        // Updating just a title must NOT require re-supplying every field.
        let out = process(&input_for(
            Some("mcp__linear__update_issue"),
            Some(json!({ "id": "FPCRM-1", "title": "Better title" })),
        ));
        assert!(
            !is_denied(&out),
            "a partial update that doesn't clear fields must pass"
        );
    }

    #[test]
    fn update_clearing_estimate_denied() {
        // But you may NOT null out a required field via update.
        let out = process(&input_for(
            Some("mcp__linear__update_issue"),
            Some(json!({ "id": "FPCRM-1", "estimate": null })),
        ));
        assert!(is_denied(&out), "nulling estimate on update must be denied");
    }

    #[test]
    fn update_priority_zero_denied() {
        let out = process(&input_for(
            Some("mcp__linear__update_issue"),
            Some(json!({ "id": "FPCRM-1", "priority": 0 })),
        ));
        assert!(
            is_denied(&out),
            "setting priority to 0 on update must be denied"
        );
    }

    #[test]
    fn read_and_discovery_linear_tools_pass_through() {
        // Read/discovery Linear tools must NEVER be gated - only create/update.
        for tool in [
            "mcp__linear__search",
            "mcp__linear__get_issue",
            "mcp__linear__list_labels",
            "mcp__linear__create_comment",
            "mcp__linear__add_issue_label",
        ] {
            let out = process(&input_for(Some(tool), Some(json!({}))));
            assert!(
                !is_denied(&out),
                "{tool} must pass through (read/discovery, not a ticket write)"
            );
        }
    }

    #[test]
    fn non_linear_tools_pass_through() {
        // The humane invariant: non-Linear work is never touched.
        for tool in ["Bash", "Write", "Edit", "mcp__github__create_issue", "Read"] {
            let out = process(&input_for(Some(tool), Some(json!({ "estimate": null }))));
            assert!(
                !is_denied(&out),
                "{tool} must pass through (not a Linear ticket write)"
            );
        }
    }

    #[test]
    fn missing_tool_name_allows() {
        let out = process(&input_for(None, None));
        assert!(!is_denied(&out));
    }

    #[test]
    fn missing_tool_input_denies_ticket_write() {
        let out = process(&input_for(Some("mcp__linear__create_issue"), None));
        assert!(is_denied(&out), "no tool_input must not authorize a write");
        let r = deny_text(&out).unwrap();
        assert!(r.contains("cannot be proven dev-ready"), "{r}");
        assert!(r.contains("missing tool_input"), "{r}");
        assert!(r.starts_with("[Sentinel-Authority]"), "{r}");
    }

    #[test]
    fn non_object_tool_input_denies_ticket_write() {
        let out = process(&input_for(
            Some("mcp__linear__update_issue"),
            Some(json!("not an object")),
        ));
        assert!(
            is_denied(&out),
            "non-object input must not authorize a write"
        );
        let r = deny_text(&out).unwrap();
        assert!(r.contains("tool_input must be a JSON object"), "{r}");
    }

    #[test]
    fn invalid_priority_type_denied_explicitly() {
        let mut p = ready_create();
        p["priority"] = json!("urgent");
        let out = process(&input_for(Some("mcp__linear__create_issue"), Some(p)));
        assert!(is_denied(&out));
        assert!(deny_text(&out)
            .unwrap()
            .contains("priority must be an integer"));
    }
}
