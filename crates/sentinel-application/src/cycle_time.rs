//! Cycle-Time Event Capture (SENTINEL-1)
//!
//! Persists Linear `Issue.update` state transitions to
//! `~/.claude/sentinel/metrics/cycle-time.jsonl`. The downstream consumers are:
//!   * SENTINEL-2 — percentile-based stale-ticket thresholds
//!   * SENTINEL-3 — cycle-time prediction at Phase 1 fetch
//!   * SENTINEL-5 — throughput operational summaries
//!   * SENTINEL-17 — per-stage cycle time breakdown
//!
//! The capture happens as a side effect inside the Hookdeck webhook pipeline
//! (`channel_events::channel_event_from_webhook`). When a Linear webhook body
//! describes a state change, we extract a structured [`CycleTimeEvent`] and
//! append a JSON line. Non-state-transition Linear events (assignee changes,
//! comments, etc.) are ignored here.
//!
//! ## On-disk schema
//!
//! One JSON object per line at
//! `~/.claude/sentinel/metrics/cycle-time.jsonl`:
//!
//! ```json
//! {
//!   "issue_id": "FPCRM-329",
//!   "team": "FPCRM",
//!   "from_state": "Code Review",
//!   "to_state": "QA Testing",
//!   "timestamp": "2026-05-02T14:23:11Z",
//!   "estimate": 3,
//!   "priority": 2,
//!   "labels": ["bug", "p1"]
//! }
//! ```
//!
//! `from_state` may be null when Linear's webhook only carries the prior
//! `stateId` without the human-readable name — we keep the row anyway because
//! the `to_state` half is still useful for throughput counts.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// One state-transition row written to `cycle-time.jsonl`.
///
/// Mirrors the schema in the SEN-1 task description verbatim. Field names use
/// `snake_case` so downstream parsers (Rust, Python, Node) all stay
/// case-insensitive-friendly without per-language overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleTimeEvent {
    /// Linear human identifier (e.g. `FPCRM-329`).
    pub issue_id: String,
    /// Linear team key (e.g. `FPCRM`). Best-effort — null when the webhook
    /// payload omits the team object (older events sometimes do).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Previous workflow state name. Null when Linear sent only the prior
    /// stateId without the nested state object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_state: Option<String>,
    /// New workflow state name. Required — if we cannot determine it, the
    /// event is rejected upstream rather than written with a placeholder.
    pub to_state: String,
    /// Timestamp the transition occurred (RFC3339). Sourced from the
    /// webhook's `updatedAt` / `createdAt` field, falling back to "now"
    /// only when neither is present.
    pub timestamp: String,
    /// Story-point estimate at the time of the transition (Linear stores
    /// this as a float but we round to integer points).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<u32>,
    /// Linear priority (0=None, 1=Urgent, 2=High, 3=Normal, 4=Low).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// Label names attached to the issue at the time of the transition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

/// Try to extract a [`CycleTimeEvent`] from a raw Hookdeck webhook body.
///
/// Returns `None` when the payload does not describe a Linear issue state
/// transition (wrong type, wrong action, no state delta, etc.). Pure
/// function — no I/O, no environment access. Suitable for unit tests against
/// fixture payloads.
#[must_use]
pub fn extract_from_linear_webhook(body: &Value) -> Option<CycleTimeEvent> {
    if body.get("type").and_then(Value::as_str) != Some("Issue") {
        return None;
    }
    if body.get("action").and_then(Value::as_str) != Some("update") {
        return None;
    }

    let data = body.get("data")?;
    let updated_from = body.get("updatedFrom")?;

    // A state transition requires `updatedFrom.stateId` to be present —
    // Linear only includes the field when it actually changed.
    updated_from.get("stateId")?;

    let to_state = data
        .pointer("/state/name")
        .and_then(Value::as_str)?
        .to_string();
    let from_state = updated_from
        .pointer("/state/name")
        .and_then(Value::as_str)
        .map(str::to_string);

    let issue_id = data.get("identifier").and_then(Value::as_str)?.to_string();

    let team = data
        .pointer("/team/key")
        .and_then(Value::as_str)
        .map(str::to_string);

    let timestamp = data
        .get("updatedAt")
        .and_then(Value::as_str)
        .or_else(|| body.get("createdAt").and_then(Value::as_str))
        .map_or_else(|| Utc::now().to_rfc3339(), str::to_string);

    // Linear stores estimate as a float; round to integer story points.
    // Negative or absurdly large values are dropped rather than truncated.
    let estimate = data
        .get("estimate")
        .and_then(Value::as_f64)
        .map(f64::round)
        .filter(|f| (0.0..=f64::from(u32::MAX)).contains(f))
        .map(|f| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = f as u32;
            n
        });

    let priority = data
        .get("priority")
        .and_then(Value::as_u64)
        .and_then(|n| u8::try_from(n).ok());

    let labels = data
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    l.get("name")
                        .and_then(Value::as_str)
                        .or_else(|| l.as_str())
                        .map(str::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(CycleTimeEvent {
        issue_id,
        team,
        from_state,
        to_state,
        timestamp,
        estimate,
        priority,
        labels,
    })
}

/// Resolve the cycle-time JSONL path. Mirrors the `hooks::metrics_dir(home)`
/// helper used elsewhere so all metrics colocate under
/// `~/.claude/sentinel/metrics/`.
///
/// Returns `None` when `dirs::home_dir()` fails to resolve a home directory
/// (e.g. broken HOME env in a sandbox).
#[must_use]
pub fn cycle_time_jsonl_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(crate::hooks::metrics_dir(&home).join("cycle-time.jsonl"))
}

/// Append a single [`CycleTimeEvent`] as a JSON line to `cycle-time.jsonl`.
///
/// Creates the parent directory and the file when missing. Each line is
/// terminated with `\n`. The write is `O_APPEND`-equivalent so concurrent
/// writers from independent Hookdeck deliveries don't tear into each other.
///
/// # Errors
///
/// Returns an `io::Error` if the home directory cannot be resolved, the
/// parent directory cannot be created, or the append fails.
pub fn append(event: &CycleTimeEvent) -> std::io::Result<()> {
    let path = cycle_time_jsonl_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not resolve home directory for cycle-time.jsonl",
        )
    })?;
    append_to(&path, event)
}

/// Append override that writes to a caller-supplied path. Used by tests and
/// callers that want to redirect the JSONL stream (e.g. into a tmpdir).
///
/// # Errors
///
/// Same as [`append`].
pub fn append_to(path: &std::path::Path, event: &CycleTimeEvent) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_full_state_transition() {
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-329",
                "title": "Fix the thing",
                "updatedAt": "2026-05-02T14:23:11Z",
                "state": { "id": "s_qa", "name": "QA Testing" },
                "team": { "key": "FPCRM" },
                "estimate": 3.0,
                "priority": 2,
                "labels": [
                    { "name": "bug" },
                    { "name": "p1" }
                ]
            },
            "updatedFrom": {
                "stateId": "s_review",
                "state": { "id": "s_review", "name": "Code Review" }
            }
        });
        let evt = extract_from_linear_webhook(&body).expect("should extract");
        assert_eq!(evt.issue_id, "FPCRM-329");
        assert_eq!(evt.team.as_deref(), Some("FPCRM"));
        assert_eq!(evt.from_state.as_deref(), Some("Code Review"));
        assert_eq!(evt.to_state, "QA Testing");
        assert_eq!(evt.timestamp, "2026-05-02T14:23:11Z");
        assert_eq!(evt.estimate, Some(3));
        assert_eq!(evt.priority, Some(2));
        assert_eq!(evt.labels, vec!["bug".to_string(), "p1".to_string()]);
    }

    #[test]
    fn keeps_row_when_prior_state_name_missing() {
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-1",
                "state": { "name": "Code Review" }
            },
            "updatedFrom": { "stateId": "prior_uuid" }
        });
        let evt = extract_from_linear_webhook(&body).expect("should extract");
        assert!(evt.from_state.is_none());
        assert_eq!(evt.to_state, "Code Review");
    }

    #[test]
    fn rejects_non_state_update_events() {
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-1",
                "title": "x",
                "state": { "name": "Todo" }
            },
            "updatedFrom": { "title": "old title" }
        });
        assert!(extract_from_linear_webhook(&body).is_none());
    }

    #[test]
    fn rejects_non_issue_events() {
        let body = json!({
            "action": "update",
            "type": "Comment",
            "data": { "id": "c_1" },
            "updatedFrom": { "stateId": "x" }
        });
        assert!(extract_from_linear_webhook(&body).is_none());
    }

    #[test]
    fn rejects_create_events() {
        let body = json!({
            "action": "create",
            "type": "Issue",
            "data": { "identifier": "FPCRM-9", "state": { "name": "Backlog" } },
            "updatedFrom": { "stateId": "x" }
        });
        assert!(extract_from_linear_webhook(&body).is_none());
    }

    #[test]
    fn rejects_when_to_state_name_unknown() {
        // Without a name we can't anchor the row meaningfully — drop it.
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-1",
                "state": { "id": "s_x" }
            },
            "updatedFrom": {
                "stateId": "old",
                "state": { "name": "Backlog" }
            }
        });
        assert!(extract_from_linear_webhook(&body).is_none());
    }

    #[test]
    fn label_array_of_strings_also_works() {
        // Some webhook fixtures sent labels as plain strings rather than
        // `{name: ...}` objects. Tolerate both shapes.
        let body = json!({
            "action": "update",
            "type": "Issue",
            "data": {
                "identifier": "FPCRM-7",
                "state": { "name": "Done" },
                "labels": ["bug", "p1"]
            },
            "updatedFrom": { "stateId": "x" }
        });
        let evt = extract_from_linear_webhook(&body).unwrap();
        assert_eq!(evt.labels, vec!["bug".to_string(), "p1".to_string()]);
    }

    #[test]
    fn missing_optional_fields_serialize_compactly() {
        let evt = CycleTimeEvent {
            issue_id: "FPCRM-1".into(),
            team: None,
            from_state: None,
            to_state: "Done".into(),
            timestamp: "2026-05-02T00:00:00Z".into(),
            estimate: None,
            priority: None,
            labels: vec![],
        };
        let s = serde_json::to_string(&evt).unwrap();
        // None / empty fields should be skipped, not serialized as null/[].
        assert!(!s.contains("team"));
        assert!(!s.contains("from_state"));
        assert!(!s.contains("estimate"));
        assert!(!s.contains("priority"));
        assert!(!s.contains("labels"));
        assert!(s.contains("\"to_state\":\"Done\""));
    }

    #[test]
    fn append_round_trips_multiple_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle-time.jsonl");

        let e1 = CycleTimeEvent {
            issue_id: "FPCRM-1".into(),
            team: Some("FPCRM".into()),
            from_state: Some("Backlog".into()),
            to_state: "In Progress".into(),
            timestamp: "2026-05-02T10:00:00Z".into(),
            estimate: Some(2),
            priority: Some(3),
            labels: vec!["bug".into()],
        };
        let e2 = CycleTimeEvent {
            issue_id: "FPCRM-2".into(),
            team: None,
            from_state: None,
            to_state: "Done".into(),
            timestamp: "2026-05-02T11:00:00Z".into(),
            estimate: None,
            priority: None,
            labels: vec![],
        };

        append_to(&path, &e1).unwrap();
        append_to(&path, &e2).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed_1: CycleTimeEvent = serde_json::from_str(lines[0]).unwrap();
        let parsed_2: CycleTimeEvent = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(parsed_1, e1);
        assert_eq!(parsed_2, e2);
    }
}
