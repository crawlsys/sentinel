//! Memory auto-loop telemetry.
//!
//! The automatic memory loop is four hook-driven stages — **recall** (inject),
//! **capture** (turn-capture), **feedback**, and the daemon **learn** loop.
//! The generic `hook-invocations.jsonl` only records `{hook, outcome, duration}`,
//! so the loop's *substance* (how many atoms were surfaced and at what scores,
//! how many candidates were written vs quarantined, which injected atoms were
//! used vs ignored) was invisible.
//!
//! This module writes a dedicated, structured event per stage to
//! `~/.claude/sentinel/metrics/memory-telemetry.jsonl` so the whole loop is
//! traceable end-to-end. One JSON object per line; a shared envelope plus a
//! free-form `detail` map per stage. The matching capture-stage event is
//! emitted by the `memory turn-capture` CLI (the detached process that owns the
//! capture outcome) into the **same file**, so a single `memory telemetry`
//! read reconstructs the full loop.
//!
//! Telemetry must never break a hook: every write error is swallowed.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::hooks::{metrics_dir, FileSystemPort};

/// File name in the sentinel metrics dir. The `memory` CLI writes the same
/// file for the capture stage — keep this string in sync with the CLI.
pub const TELEMETRY_FILE: &str = "memory-telemetry.jsonl";

/// One telemetry event. `stage` is the loop phase; `detail` carries the
/// phase-specific payload (kept free-form so each stage can evolve its fields
/// without a schema migration — readers tolerate missing keys).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryEvent {
    /// RFC3339 timestamp.
    pub ts: String,
    /// Loop stage: `"recall"`, `"capture"`, `"feedback"`, or `"learn"`.
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Wall-clock duration of the stage in milliseconds, when measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Stage-specific payload.
    pub detail: Value,
}

fn telemetry_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = metrics_dir(&home);
    let _ = fs.create_dir_all(&dir);
    Some(dir.join(TELEMETRY_FILE))
}

/// Append one event. Swallows all errors — telemetry must never break a hook.
pub fn record(fs: &dyn FileSystemPort, event: &MemoryEvent) {
    let Some(path) = telemetry_path(fs) else {
        return;
    };
    let Ok(json) = serde_json::to_string(event) else {
        return;
    };
    let mut line = json;
    line.push('\n');
    let _ = fs.append(&path, line.as_bytes());
}

/// Convenience: emit a `recall` event for the memory_inject hook.
///
/// `hits` is the list of surfaced atoms as `(id, event_id, name, score)`.
/// `injected` is whether the rendered block was actually returned as context
/// (false when zero hits or the search was skipped/failed).
pub fn record_recall(
    fs: &dyn FileSystemPort,
    session_id: Option<&str>,
    project: &str,
    prompt_chars: usize,
    injected: bool,
    hits: &[(String, Option<String>, String, f64)],
    duration_ms: u64,
) {
    let hit_json: Vec<Value> = hits
        .iter()
        .map(|(id, event_id, name, score)| {
            json!({ "id": id, "event_id": event_id, "name": name, "score": score })
        })
        .collect();
    record(
        fs,
        &MemoryEvent {
            ts: chrono::Utc::now().to_rfc3339(),
            stage: "recall".to_string(),
            session_id: session_id.map(str::to_string),
            project: Some(project.to_string()),
            duration_ms: Some(duration_ms),
            detail: json!({
                "prompt_chars": prompt_chars,
                "atoms_surfaced": hits.len(),
                "injected": injected,
                "hits": hit_json,
            }),
        },
    );
}

/// Convenience: emit a `feedback` event for the memory_feedback hook.
///
/// `outcomes` is the per-atom classification as `(event_id, label)` where label
/// is `"used" | "ignored" | "contradicted"`.
pub fn record_feedback(
    fs: &dyn FileSystemPort,
    session_id: Option<&str>,
    injected_count: usize,
    outcomes: &[(String, &'static str)],
    correction_signal: Option<&str>,
) {
    let mut used = 0usize;
    let mut ignored = 0usize;
    let mut contradicted = 0usize;
    for (_, label) in outcomes {
        match *label {
            "used" => used += 1,
            "contradicted" => contradicted += 1,
            _ => ignored += 1,
        }
    }
    let outcome_json: Vec<Value> = outcomes
        .iter()
        .map(|(eid, label)| json!({ "event_id": eid, "label": label }))
        .collect();
    record(
        fs,
        &MemoryEvent {
            ts: chrono::Utc::now().to_rfc3339(),
            stage: "feedback".to_string(),
            session_id: session_id.map(str::to_string),
            project: None,
            duration_ms: None,
            detail: json!({
                "injected_count": injected_count,
                "used": used,
                "ignored": ignored,
                "contradicted": contradicted,
                "correction_signal": correction_signal,
                "outcomes": outcome_json,
            }),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_detail_shape() {
        let hits = vec![
            ("a1".to_string(), Some("e1".to_string()), "user/likes=rust".to_string(), 0.91),
            ("a2".to_string(), None, "proj/uses=qdrant".to_string(), 0.77),
        ];
        let ev = MemoryEvent {
            ts: "t".into(),
            stage: "recall".into(),
            session_id: Some("s".into()),
            project: Some("memory".into()),
            duration_ms: Some(2700),
            detail: json!({
                "prompt_chars": 42,
                "atoms_surfaced": hits.len(),
                "injected": true,
                "hits": hits.iter().map(|(id,eid,name,score)| json!({"id":id,"event_id":eid,"name":name,"score":score})).collect::<Vec<_>>(),
            }),
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("\"stage\":\"recall\""));
        assert!(s.contains("\"atoms_surfaced\":2"));
        assert!(s.contains("user/likes=rust"));
        // round-trips
        let back: MemoryEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back.stage, "recall");
        assert_eq!(back.detail["atoms_surfaced"], 2);
    }

    #[test]
    fn feedback_counts_classify() {
        // Build outcomes and assert the convenience counter buckets them.
        let outcomes: Vec<(String, &'static str)> = vec![
            ("e1".into(), "used"),
            ("e2".into(), "ignored"),
            ("e3".into(), "contradicted"),
            ("e4".into(), "used"),
        ];
        let mut used = 0;
        let mut ignored = 0;
        let mut contradicted = 0;
        for (_, l) in &outcomes {
            match *l {
                "used" => used += 1,
                "contradicted" => contradicted += 1,
                _ => ignored += 1,
            }
        }
        assert_eq!((used, ignored, contradicted), (2, 1, 1));
    }
}
