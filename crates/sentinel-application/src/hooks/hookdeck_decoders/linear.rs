//! Linear webhook decoders.
//!
//! Payload shape (Linear docs + observed fixtures):
//!
//! ```json
//! { "action": "update" | "create" | "remove",
//!   "type": "Issue" | "Comment" | "IssueLabel" | "Reaction" | "Project" | ...,
//!   "data": { /* entity snapshot AFTER the change */ },
//!   "updatedFrom": { /* only present for action="update" — fields that changed */ },
//!   "url": "https://linear.app/...",
//!   "createdAt": "2026-04-23T...",
//!   "actor": { "id": "...", "name": "QA reviewer" } }
//! ```
//!
//! For `Issue.update` state transitions we diff `data.state` against
//! `updatedFrom.stateId` → look up the prior state name via `data.state.name`
//! if present in the payload (Linear sends both `stateId` and the nested
//! `state` object so the summary can name both endpoints).

use serde_json::Value;

use super::{truncate_inline, Decoded};

pub fn decode(body: &Value) -> Option<Decoded> {
    let entity = body.get("type").and_then(Value::as_str)?;
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("event");
    let data = body.get("data").unwrap_or(&Value::Null);

    let summary = match entity {
        "Issue" => decode_issue(action, data, body),
        "Comment" => decode_comment(action, data, body),
        "IssueLabel" => decode_issue_label(action, data, body),
        "Reaction" => decode_reaction(action, data, body),
        "Project" => decode_project(action, data, body),
        "ProjectUpdate" => decode_project_update(action, data, body),
        "Cycle" => decode_cycle(action, data, body),
        "Attachment" => decode_attachment(action, data, body),
        _ => None,
    };

    summary.map(|s| Decoded::new(format!("[LINEAR] {s}"), body))
}

fn actor_name(body: &Value) -> Option<String> {
    body.pointer("/actor/name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn issue_identifier(data: &Value) -> String {
    data.get("identifier")
        .and_then(Value::as_str)
        .unwrap_or("<issue>")
        .to_string()
}

fn decode_issue(action: &str, data: &Value, body: &Value) -> Option<String> {
    let id = issue_identifier(data);
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    let actor = actor_name(body);

    match action {
        "create" => Some(format_with_actor(
            &format!("{id} created: {}", truncate_inline(title, 80)),
            actor.as_deref(),
        )),
        "remove" => Some(format_with_actor(
            &format!("{id} deleted: {}", truncate_inline(title, 80)),
            actor.as_deref(),
        )),
        "update" => {
            let updated_from = body.get("updatedFrom");
            if let Some(line) = issue_state_transition(&id, data, updated_from) {
                return Some(format_with_actor(&line, actor.as_deref()));
            }
            if let Some(line) = issue_assignee_change(&id, data, updated_from) {
                return Some(format_with_actor(&line, actor.as_deref()));
            }
            if let Some(line) = issue_priority_change(&id, data, updated_from) {
                return Some(format_with_actor(&line, actor.as_deref()));
            }
            if let Some(line) = issue_title_change(&id, data, updated_from) {
                return Some(format_with_actor(&line, actor.as_deref()));
            }
            Some(format_with_actor(
                &format!("{id} updated: {}", truncate_inline(title, 80)),
                actor.as_deref(),
            ))
        }
        other => Some(format_with_actor(
            &format!("{id} {other}: {}", truncate_inline(title, 80)),
            actor.as_deref(),
        )),
    }
}

fn issue_state_transition(id: &str, data: &Value, updated_from: Option<&Value>) -> Option<String> {
    let updated_from = updated_from?;
    // `updatedFrom.stateId` present means the state changed.
    updated_from.get("stateId")?;

    // Linear sends the new state object nested, and (for state changes) the old
    // state name in `updatedFrom.state.name` is NOT included — we have only the
    // old stateId. Fall back to "→ <new>" if we can't name the prior state.
    let new_state = data
        .pointer("/state/name")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let old_state = updated_from.pointer("/state/name").and_then(Value::as_str);

    Some(match old_state {
        Some(old) => format!("{id} moved {old} → {new_state}"),
        None => format!("{id} moved → {new_state}"),
    })
}

fn issue_assignee_change(id: &str, data: &Value, updated_from: Option<&Value>) -> Option<String> {
    let updated_from = updated_from?;
    updated_from.get("assigneeId")?;

    let new_name = data.pointer("/assignee/name").and_then(Value::as_str);
    Some(match new_name {
        Some(n) => format!("{id} reassigned to {n}"),
        None => format!("{id} unassigned"),
    })
}

fn issue_priority_change(id: &str, data: &Value, updated_from: Option<&Value>) -> Option<String> {
    let updated_from = updated_from?;
    updated_from.get("priority")?;
    let p = data
        .get("priorityLabel")
        .and_then(Value::as_str)
        .or_else(|| {
            data.get("priority")
                .and_then(Value::as_u64)
                .map(|_| "updated")
        })?;
    Some(format!("{id} priority → {p}"))
}

fn issue_title_change(id: &str, data: &Value, updated_from: Option<&Value>) -> Option<String> {
    let updated_from = updated_from?;
    updated_from.get("title")?;
    let new_title = data.get("title").and_then(Value::as_str)?;
    Some(format!("{id} retitled: {}", truncate_inline(new_title, 80)))
}

fn decode_comment(action: &str, data: &Value, body: &Value) -> Option<String> {
    // Comment payloads nest the parent issue at /data/issue.
    let issue_id = data
        .pointer("/issue/identifier")
        .and_then(Value::as_str)
        .unwrap_or("<issue>");
    let comment_body = data
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("(empty)");
    let actor = actor_name(body);

    let verb = match action {
        "create" => "commented",
        "update" => "edited a comment",
        "remove" => "deleted a comment",
        other => other,
    };

    let excerpt = truncate_inline(comment_body, 120);
    let base = match (actor.as_deref(), action) {
        (Some(name), "create") => format!("{name} commented on {issue_id}: \"{excerpt}\""),
        (None, "create") => format!("{issue_id} comment: \"{excerpt}\""),
        (Some(name), _) => format!("{name} {verb} on {issue_id}: \"{excerpt}\""),
        (None, _) => format!("{issue_id} {verb}: \"{excerpt}\""),
    };
    Some(base)
}

fn decode_issue_label(action: &str, data: &Value, body: &Value) -> Option<String> {
    // IssueLabel events fire when a label is added to or removed from an issue.
    // Payload shape varies — look for issue.identifier and label name.
    let issue_id = data
        .pointer("/issue/identifier")
        .and_then(Value::as_str)
        .or_else(|| data.get("identifier").and_then(Value::as_str))
        .unwrap_or("<issue>");
    let label_name = data
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| data.pointer("/label/name").and_then(Value::as_str))
        .unwrap_or("<label>");
    let actor = actor_name(body);

    let line = match action {
        "create" => format!("{issue_id} labeled with \"{label_name}\""),
        "remove" => format!("{issue_id} unlabeled \"{label_name}\""),
        other => format!("{issue_id} label {other}: \"{label_name}\""),
    };
    Some(format_with_actor(&line, actor.as_deref()))
}

fn decode_reaction(action: &str, data: &Value, body: &Value) -> Option<String> {
    // Reactions fire on Comments. Parent issue at /data/comment/issue.
    let issue_id = data
        .pointer("/comment/issue/identifier")
        .and_then(Value::as_str)
        .unwrap_or("<issue>");
    let emoji = data
        .get("emoji")
        .and_then(Value::as_str)
        .unwrap_or("reaction");
    let actor = actor_name(body);

    let line = match action {
        "create" => format!("{emoji} reaction added on {issue_id} comment"),
        "remove" => format!("{emoji} reaction removed on {issue_id} comment"),
        other => format!("reaction {other} on {issue_id} comment"),
    };
    Some(format_with_actor(&line, actor.as_deref()))
}

fn decode_project(action: &str, data: &Value, body: &Value) -> Option<String> {
    let name = data
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("<project>");
    let actor = actor_name(body);
    let line = format!("Project {action}: {}", truncate_inline(name, 80));
    Some(format_with_actor(&line, actor.as_deref()))
}

fn decode_project_update(action: &str, data: &Value, body: &Value) -> Option<String> {
    let project = data
        .pointer("/project/name")
        .and_then(Value::as_str)
        .unwrap_or("<project>");
    let health = data.get("health").and_then(Value::as_str);
    let actor = actor_name(body);

    let line = match health {
        Some(h) => format!("ProjectUpdate {action} on {project} (health: {h})"),
        None => format!("ProjectUpdate {action} on {project}"),
    };
    Some(format_with_actor(&line, actor.as_deref()))
}

fn decode_cycle(action: &str, data: &Value, body: &Value) -> Option<String> {
    let number = data
        .get("number")
        .and_then(Value::as_u64)
        .map_or_else(|| "Cycle".to_string(), |n| format!("Cycle {n}"));
    let actor = actor_name(body);
    let line = format!("{number} {action}");
    Some(format_with_actor(&line, actor.as_deref()))
}

fn decode_attachment(action: &str, data: &Value, body: &Value) -> Option<String> {
    let issue_id = data
        .pointer("/issue/identifier")
        .and_then(Value::as_str)
        .unwrap_or("<issue>");
    let title = data
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("attachment");
    let actor = actor_name(body);
    let line = format!(
        "{issue_id} attachment {action}: {}",
        truncate_inline(title, 80)
    );
    Some(format_with_actor(&line, actor.as_deref()))
}

fn format_with_actor(line: &str, actor: Option<&str>) -> String {
    match actor {
        Some(name) => format!("{line} (by {name})"),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode;
    use serde_json::json;

    #[test]
    fn issue_state_transition_names_both_endpoints_when_possible() {
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-329",
                "title": "Fix the thing",
                "state": { "id": "s_qa_testing", "name": "QA Testing" }
            },
            "updatedFrom": {
                "stateId": "s_qa_failed",
                "state": { "id": "s_qa_failed", "name": "QA Failed" }
            },
            "actor": { "id": "u_reviewer", "name": "QA reviewer" }
        });
        let d = decode("linear", None, &body);
        assert_eq!(
            d.summary,
            "[LINEAR] FPCRM-329 moved QA Failed → QA Testing (by QA reviewer)"
        );
    }

    #[test]
    fn issue_state_transition_falls_back_to_new_state_only() {
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-329",
                "title": "x",
                "state": { "name": "Code Review" }
            },
            "updatedFrom": { "stateId": "prior" }
        });
        let d = decode("linear", None, &body);
        assert_eq!(d.summary, "[LINEAR] FPCRM-329 moved → Code Review");
    }

    #[test]
    fn comment_create_matches_spec_format() {
        let body = json!({
            "action": "create",
            "type": "Comment",
            "data": {
                "id": "cmt_1",
                "body": "Still failing — selection not persisting after save",
                "issue": {
                    "identifier": "FPCRM-330",
                    "team": { "key": "FPCRM" }
                }
            },
            "actor": { "name": "QA reviewer" }
        });
        let d = decode("linear", None, &body);
        assert_eq!(
            d.summary,
            "[LINEAR] QA reviewer commented on FPCRM-330: \"Still failing — selection not persisting after save\""
        );
    }

    #[test]
    fn comment_body_is_truncated() {
        let body = json!({
            "action": "create",
            "type": "Comment",
            "data": {
                "body": "a".repeat(500),
                "issue": { "identifier": "FPCRM-1" }
            }
        });
        let d = decode("linear", None, &body);
        assert!(d.summary.contains("…"));
        assert!(d.summary.len() < 200);
    }

    #[test]
    fn issue_label_create_matches_spec_format() {
        let body = json!({
            "action": "create",
            "type": "IssueLabel",
            "data": {
                "name": "ci-flake",
                "issue": { "identifier": "FPCRM-398" }
            }
        });
        let d = decode("linear", None, &body);
        assert_eq!(d.summary, "[LINEAR] FPCRM-398 labeled with \"ci-flake\"");
    }

    #[test]
    fn reaction_create_names_emoji_and_issue() {
        let body = json!({
            "action": "create",
            "type": "Reaction",
            "data": {
                "emoji": "+1",
                "comment": {
                    "issue": { "identifier": "FPCRM-1" }
                }
            },
            "actor": { "name": "operator" }
        });
        let d = decode("linear", None, &body);
        assert_eq!(
            d.summary,
            "[LINEAR] +1 reaction added on FPCRM-1 comment (by operator)"
        );
    }

    #[test]
    fn issue_create_includes_title() {
        let body = json!({
            "action": "create",
            "type": "Issue",
            "data": { "identifier": "FPCRM-500", "title": "New feature" }
        });
        let d = decode("linear", None, &body);
        assert_eq!(d.summary, "[LINEAR] FPCRM-500 created: New feature");
    }

    #[test]
    fn issue_assignee_update() {
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-1",
                "title": "x",
                "assignee": { "name": "QA reviewer" }
            },
            "updatedFrom": { "assigneeId": "old_id" }
        });
        let d = decode("linear", None, &body);
        assert_eq!(d.summary, "[LINEAR] FPCRM-1 reassigned to QA reviewer");
    }

    #[test]
    fn unknown_type_falls_back() {
        let body = json!({
            "action": "create",
            "type": "SomethingNew",
            "data": { "id": "x_1" }
        });
        let d = decode("linear", None, &body);
        // Fallback path: no specific decoder → generic summary
        assert_eq!(d.summary, "[HOOKDECK:linear] SomethingNew on x_1");
    }
}
