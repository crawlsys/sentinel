//! Real-time single-issue Linear lookup adapter for the PM gate.
//!
//! Implements [`LinearLookupPort`](sentinel_domain::ports::LinearLookupPort)
//! with a **blocking** HTTP call so the synchronous `linear_pm_gate` hook can
//! query live Linear at pickup without an async runtime. It fetches exactly
//! the one ticket being started and normalizes the response into the same JSON
//! shape as the on-disk cache, so the gate's existing parsers work unchanged.
//!
//! Fast + fail-closed by design: a short timeout, and typed errors for lookup
//! failures so PM enforcement never substitutes stale local state for live
//! Linear authority.

use std::time::Duration;

use sentinel_domain::ports::{LinearLookupError, LinearLookupPort};
use serde_json::Value;

/// Linear GraphQL endpoint.
const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";
/// Per-request timeout. The gate has a tight budget and blocks the start when
/// Linear cannot answer within it.
const TIMEOUT: Duration = Duration::from_millis(2500);

/// Blocking single-issue Linear lookup. Holds a reqwest blocking client and
/// the Linear token (a Personal Access Token in the `Authorization` header).
pub struct LinearLookup {
    client: reqwest::blocking::Client,
    token: String,
}

impl LinearLookup {
    /// Build from the `SENTINEL_LINEAR_TOKEN` env var. Returns `Ok(None)` only
    /// when the token is absent; the PM gate will fail closed for targeted
    /// Linear start attempts until live authority is configured.
    pub fn from_env() -> Result<Option<Self>, LinearLookupError> {
        let Some(token) = std::env::var("SENTINEL_LINEAR_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        else {
            return Ok(None);
        };
        let client = reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|err| {
                LinearLookupError::Transport(format!("failed to build Linear HTTP client: {err}"))
            })?;
        Ok(Some(Self { client, token }))
    }

    /// Construct from an explicit token (used in tests against a mock server).
    pub fn with_token(token: String) -> Result<Self, LinearLookupError> {
        let client = reqwest::blocking::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|err| {
                LinearLookupError::Transport(format!("failed to build Linear HTTP client: {err}"))
            })?;
        Ok(Self { client, token })
    }
}

impl LinearLookupPort for LinearLookup {
    fn fetch_issue(&self, identifier: &str) -> Result<Value, LinearLookupError> {
        // Single-issue query covering every field the gate reads. Mirrors the
        // enforcer's readiness query so the two stay consistent.
        let id_literal = serde_json::to_string(identifier).map_err(|err| {
            LinearLookupError::Decode(format!("failed to encode Linear issue id: {err}"))
        })?;
        let query = format!(
            "query{{issue(id:{id_literal}){{identifier estimate priority \
description state{{name type}} labels{{nodes{{name}}}} assignee{{id displayName}} \
projectMilestone{{id name}} project{{id projectMilestones{{nodes{{id}}}}}} \
inverseRelations{{nodes{{type issue{{identifier state{{type}}}}}}}}}}}}"
        );
        let body = serde_json::json!({ "query": query });
        let resp = self
            .client
            .post(LINEAR_GRAPHQL_URL)
            .header("Authorization", &self.token)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|err| LinearLookupError::Transport(err.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LinearLookupError::Transport(format!(
                "Linear GraphQL returned HTTP {status}"
            )));
        }
        let json: Value = resp
            .json()
            .map_err(|err| LinearLookupError::Decode(err.to_string()))?;
        if let Some(errors) = json.get("errors").and_then(Value::as_array) {
            if !errors.is_empty() {
                return Err(LinearLookupError::MalformedResponse(format!(
                    "GraphQL errors for {identifier}: {errors:?}"
                )));
            }
        }
        let issue = json
            .get("data")
            .and_then(|data| data.get("issue"))
            .ok_or_else(|| {
                LinearLookupError::MalformedResponse(format!("missing data.issue for {identifier}"))
            })?;
        if issue.is_null() {
            return Err(LinearLookupError::MissingIssue(identifier.to_string()));
        }
        Ok(normalize(issue))
    }
}

/// Normalize a raw Linear `issue` object into the cache shape the gate parses:
/// flat `labels` (array of `{name}`), `blockedBy` (array of `{identifier,
/// state}`), and `projectHasMilestones` (bool) derived from the project's
/// milestone list.
fn normalize(issue: &Value) -> Value {
    let mut out = serde_json::Map::new();

    for key in ["identifier", "estimate", "priority", "description"] {
        if let Some(v) = issue.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    if let Some(state) = issue.get("state") {
        out.insert("state".into(), state.clone());
    }
    if let Some(assignee) = issue.get("assignee") {
        out.insert("assignee".into(), assignee.clone());
    }
    if let Some(pm) = issue.get("projectMilestone") {
        out.insert("projectMilestone".into(), pm.clone());
    }

    // labels{nodes{name}} → [{name}]
    if let Some(nodes) = issue
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(Value::as_array)
    {
        out.insert("labels".into(), Value::Array(nodes.clone()));
    }

    // projectHasMilestones: project.projectMilestones.nodes non-empty.
    let has_ms = issue
        .get("project")
        .and_then(|p| p.get("projectMilestones"))
        .and_then(|m| m.get("nodes"))
        .and_then(Value::as_array)
        .is_some_and(|n| !n.is_empty());
    out.insert("projectHasMilestones".into(), Value::Bool(has_ms));

    // inverseRelations of type "blocks" mean THIS issue is blocked by the
    // related issue → emit as blockedBy[{identifier, state}].
    if let Some(nodes) = issue
        .get("inverseRelations")
        .and_then(|r| r.get("nodes"))
        .and_then(Value::as_array)
    {
        let blocked_by: Vec<Value> = nodes
            .iter()
            .filter(|rel| {
                rel.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.eq_ignore_ascii_case("blocks"))
            })
            .filter_map(|rel| rel.get("issue").cloned())
            .collect();
        if !blocked_by.is_empty() {
            out.insert("blockedBy".into(), Value::Array(blocked_by));
        }
    }

    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_flattens_labels_and_milestones() {
        let raw = serde_json::json!({
            "identifier": "FPCRM-1",
            "estimate": 3,
            "priority": 2,
            "state": { "name": "Todo", "type": "backlog" },
            "labels": { "nodes": [{ "name": "frontend" }, { "name": "blocked" }] },
            "projectMilestone": null,
            "project": { "projectMilestones": { "nodes": [{ "id": "m1" }] } },
            "assignee": { "id": "u1", "displayName": "Rene" }
        });
        let n = normalize(&raw);
        assert_eq!(n.get("identifier").unwrap(), "FPCRM-1");
        assert_eq!(n.get("priority").unwrap(), 2);
        assert_eq!(n.get("projectHasMilestones").unwrap(), true);
        let labels = n.get("labels").unwrap().as_array().unwrap();
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[1].get("name").unwrap(), "blocked");
    }

    #[test]
    fn normalize_emits_blocked_by_from_inverse_relations() {
        let raw = serde_json::json!({
            "identifier": "FPCRM-2",
            "inverseRelations": { "nodes": [
                { "type": "blocks", "issue": { "identifier": "FPCRM-9", "state": { "type": "started" } } },
                { "type": "related", "issue": { "identifier": "FPCRM-3", "state": { "type": "backlog" } } }
            ]}
        });
        let n = normalize(&raw);
        let bb = n.get("blockedBy").unwrap().as_array().unwrap();
        assert_eq!(bb.len(), 1); // only the "blocks" relation
        assert_eq!(bb[0].get("identifier").unwrap(), "FPCRM-9");
    }

    #[test]
    fn normalize_no_milestones_is_false() {
        let raw = serde_json::json!({
            "identifier": "FPCRM-3",
            "project": { "projectMilestones": { "nodes": [] } }
        });
        let n = normalize(&raw);
        assert_eq!(n.get("projectHasMilestones").unwrap(), false);
    }
}
