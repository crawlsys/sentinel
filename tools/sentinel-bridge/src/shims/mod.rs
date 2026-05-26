//! Per-harness shims. Each shim tails a non-Claude harness's native
//! session output, translates it into hook-invocation JSONL records,
//! and writes them to ~/.{harness}/sentinel/metrics/hook-invocations.jsonl
//! where the bridge's main ingest loop picks them up.
//!
//! Ports of the retired Python shims at
//! tools/sentinel-viz/harness-shims/*.py — same semantics, same
//! output format, same state-file schema.

pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod qwen;

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

/// Emit one hook-invocation record to `out`. Format mirrors the
/// Python shims and Claude's native hook JSONL so a single bridge
/// ingest loop covers both.
pub fn emit_record(
    out: &Path,
    event: &str,
    hook: &str,
    session_id: &str,
    ts: &str,
    repo_root: &str,
    source_harness: &str,
) -> Result<()> {
    if let Some(parent) = out.parent() {
        create_dir_all(parent).ok();
    }
    let trace_id = Uuid::new_v4().to_string();
    let rec = json!({
        "event": event,
        "hook": hook,
        "outcome": "allow",
        "repo_root": repo_root,
        "session_id": session_id,
        "trace_id": trace_id,
        "ts": ts,
        "duration_us": 0,
        "source_harness": source_harness,
    });
    let mut f = OpenOptions::new().create(true).append(true).open(out)?;
    writeln!(f, "{}", serde_json::to_string(&rec)?)?;
    Ok(())
}

#[allow(dead_code)]
pub fn ms_to_iso(ms: i64) -> String {
    let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
