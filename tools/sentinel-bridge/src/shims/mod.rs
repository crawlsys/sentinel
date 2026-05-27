//! Per-harness shims. Each shim tails a non-Claude harness's native
//! session output, translates it into hook-invocation JSONL records,
//! and writes them to ~/.{harness}/sentinel/metrics/hook-invocations.jsonl
//! where the bridge's main ingest loop picks them up.
//!
//! Ports of the retired Python shims at
//! tools/sentinel-viz/harness-shims/*.py — same semantics, same
//! output format, same state-file schema.
//!
//! Active: codex only. opencode/qwen/gemini are gated dormant via
//! `feature = "extra-harnesses"` (the workspace doesn't enable it) —
//! the files stay compilable on their own for the option to revive
//! them, but the bridge CLI no longer exposes them. See main.rs's
//! Shim enum for the allowlist rationale.

pub mod codex;

#[cfg(feature = "extra-harnesses")]
pub mod gemini;
#[cfg(feature = "extra-harnesses")]
pub mod opencode;
#[cfg(feature = "extra-harnesses")]
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
///
/// `tool` is the Claude-normalized tool name (Bash / Read / Edit /
/// TaskUpdate / etc.). Per-harness shims call into [`normalize_tool`]
/// to map their native tool taxonomy onto this set before calling
/// here. Pass `""` for events that don't carry a tool (UserPromptSubmit,
/// SessionStart, Stop).
#[allow(clippy::too_many_arguments)]
pub fn emit_record(
    out: &Path,
    event: &str,
    hook: &str,
    session_id: &str,
    ts: &str,
    repo_root: &str,
    source_harness: &str,
    tool: &str,
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
        "tool": tool,
    });
    let mut f = OpenOptions::new().create(true).append(true).open(out)?;
    writeln!(f, "{}", serde_json::to_string(&rec)?)?;
    Ok(())
}

/// Map a harness-native tool name into the Claude tool taxonomy the
/// dashboard's categorizer expects (Bash / Read / Write / Edit /
/// MultiEdit / Grep / Glob / TaskUpdate / TaskList / Agent /
/// AskUserQuestion / WebFetch / WebSearch / NotebookEdit /
/// ToolSearch). Unrecognised inputs pass through unchanged.
pub fn normalize_tool(harness: &str, raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let mapped = match (harness, lower.as_str()) {
        // codex (~/.codex)
        (_, "exec_command") | (_, "exec") | (_, "shell") => "Bash",
        (_, "apply_patch") | (_, "edit_file") | (_, "patch") => "Edit",
        (_, "read_file") | (_, "cat") => "Read",
        (_, "write_file") => "Write",
        (_, "update_plan") | (_, "todo_write") => "TaskUpdate",
        (_, "todo_read") | (_, "todo_list") => "TaskList",
        (_, "web_search") => "WebSearch",
        (_, "web_fetch") | (_, "fetch_url") => "WebFetch",
        // opencode native (mostly lowercase claude-likes)
        (_, "bash") => "Bash",
        (_, "read") => "Read",
        (_, "write") => "Write",
        (_, "edit") => "Edit",
        (_, "multiedit") => "MultiEdit",
        (_, "grep") => "Grep",
        (_, "glob") => "Glob",
        (_, "task") => "Agent",
        (_, "ask") | (_, "ask_user") => "AskUserQuestion",
        (_, "notebookedit") | (_, "notebook_edit") => "NotebookEdit",
        // qwen specifics
        (_, "list_directory") | (_, "ls") => "Glob",
        (_, "search_file_content") => "Grep",
        _ => "",
    };
    if mapped.is_empty() {
        raw.to_string()
    } else {
        mapped.to_string()
    }
}

#[allow(dead_code)]
pub fn ms_to_iso(ms: i64) -> String {
    let dt: DateTime<Utc> = Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now);
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
