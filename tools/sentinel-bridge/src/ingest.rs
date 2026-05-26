//! Core hook + session ingestion. Mirrors `_ingest_hooks` /
//! `_ingest_sessions` in the retired `sentinel_bridge.py`.
//!
//! Each metrics directory contributes two JSONLs:
//!   - hook-invocations.jsonl  (one Claude hook event per line)
//!   - sessions.jsonl          (session lifecycle markers — claude-only)
//!
//! We tail both incrementally via the offset state in jsonl.rs.

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::jsonl::{read_new, OffsetState};
use crate::store::{HookData, SessionData, Store};

/// Canonical metrics dirs. Mirrors METRICS_DIRS in sentinel_bridge.py.
/// First two are claude-native; the rest are per-harness shims.
pub fn metrics_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    vec![
        home.join(".claude").join("sentinel").join("metrics"),
        home.join(".claude-sentinel").join("sentinel").join("metrics"),
        home.join(".codex").join("sentinel").join("metrics"),
        home.join(".opencode").join("sentinel").join("metrics"),
        home.join(".qwen").join("sentinel").join("metrics"),
        home.join(".gemini").join("sentinel").join("metrics"),
    ]
}

pub fn store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".agents")
        .join("scratch")
        .join("activegraph-bridge")
        .join("sentinel.db")
}

pub fn offset_state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".agents")
        .join("scratch")
        .join("activegraph-bridge")
        .join("bridge.state.json")
}

/// One pass over all metrics dirs. Reads only new bytes since the
/// last successful pass (tracked in OffsetState). Returns the number
/// of new hook records ingested.
pub fn run_pass(store: &mut Store, state: &mut OffsetState) -> Result<usize> {
    let mut emitted = 0;
    store.begin()?;

    for dir in metrics_dirs() {
        let sessions_path = dir.join("sessions.jsonl");
        let hooks_path = dir.join("hook-invocations.jsonl");

        // Sessions first: every session.jsonl record is a
        // session_start / session_end marker; we materialise the
        // SentinelSession objects up-front so the hooks below can
        // resolve their parent.
        if sessions_path.exists() {
            let key = sessions_path.to_string_lossy().into_owned();
            let off = state.offsets.get(&key).copied().unwrap_or(0);
            let (records, new_off) = read_new(&sessions_path, off)?;
            for rec in records {
                if let Some(sd) = parse_session_record(&rec, "claude") {
                    store.upsert_session(&sd)?;
                }
            }
            state.offsets.insert(key, new_off);
        }

        // Hooks: each line is a hook invocation. Create a session
        // stub if the session_id hasn't been seen, then materialise
        // the SentinelHookInvocation + has_invocation relation.
        if hooks_path.exists() {
            let key = hooks_path.to_string_lossy().into_owned();
            let off = state.offsets.get(&key).copied().unwrap_or(0);
            let (records, new_off) = read_new(&hooks_path, off)?;
            for rec in records {
                if let Some(hd) = parse_hook_record(&rec) {
                    let session_obj_id = match store.lookup_session_obj_id(&hd.session_id)? {
                        Some(id) => id,
                        None => {
                            // Synthesise a session from the hook's
                            // own metadata. Mirrors the Python stub
                            // creation path.
                            let stub = SessionData {
                                session_id: hd.session_id.clone(),
                                cwd: hd.repo_root.clone(),
                                platform: String::new(),
                                started_at: hd.ts.clone(),
                                source_harness: hd.source_harness.clone(),
                            };
                            store.upsert_session(&stub)?
                        }
                    };
                    store.ingest_hook(&hd, &session_obj_id)?;
                    emitted += 1;
                }
            }
            state.offsets.insert(key, new_off);
        }
    }

    store.commit()?;
    Ok(emitted)
}

fn parse_session_record(v: &Value, default_harness: &str) -> Option<SessionData> {
    let sid = v.get("session_id")?.as_str()?.to_string();
    Some(SessionData {
        session_id: sid,
        cwd: v.get("cwd").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        platform: v
            .get("platform")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        started_at: v
            .get("ts")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        source_harness: v
            .get("source_harness")
            .and_then(|x| x.as_str())
            .unwrap_or(default_harness)
            .to_string(),
    })
}

fn parse_hook_record(v: &Value) -> Option<HookData> {
    let session_id = v.get("session_id")?.as_str()?.to_string();
    let trace_id = v.get("trace_id")?.as_str()?.to_string();
    let event = v.get("event")?.as_str()?.to_string();
    Some(HookData {
        hook: v.get("hook").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        event,
        outcome: v
            .get("outcome")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        session_id,
        trace_id,
        duration_us: v.get("duration_us").and_then(|x| x.as_u64()).unwrap_or(0),
        repo_root: v
            .get("repo_root")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        ts: v.get("ts").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        source_harness: v
            .get("source_harness")
            .and_then(|x| x.as_str())
            .unwrap_or("claude")
            .to_string(),
        tool: v.get("tool").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

#[allow(dead_code)]
pub fn touch_metrics_dirs() {
    for dir in metrics_dirs() {
        let _ = std::fs::create_dir_all(&dir);
    }
}

#[allow(dead_code)]
pub fn dir_for(_path: &Path) -> Option<PathBuf> {
    None
}
