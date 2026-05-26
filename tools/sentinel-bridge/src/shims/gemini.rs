//! gemini_shim — translate ~/.gemini/tmp/<project>/logs.json into
//! bridge hook-invocation records. Port of harness-shims/gemini_shim.py.
//!
//! Gemini's surface today is only user-message events (no tool calls).
//! On messageId == 0 we emit SessionStart; every user event becomes
//! UserPromptSubmit.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::jsonl::read_full_array;
use crate::shims::emit_record;

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// session_id → max messageId seen
    seen: HashMap<String, i64>,
}

fn out_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("sentinel")
        .join("metrics")
        .join("hook-invocations.jsonl")
}

fn state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".gemini")
        .join("sentinel")
        .join("metrics")
        .join("gemini-shim.state.json")
}

fn tmp_root() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".gemini").join("tmp")
}

fn load_state() -> State {
    if let Ok(raw) = fs::read_to_string(state_path()) {
        if let Ok(s) = serde_json::from_str::<State>(&raw) {
            return s;
        }
    }
    State::default()
}

fn save_state(s: &State) -> Result<()> {
    let path = state_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    fs::write(&path, serde_json::to_string_pretty(s)?)?;
    Ok(())
}

pub fn run_once() -> Result<usize> {
    let pattern = format!("{}/*/logs.json", tmp_root().display());
    let files = crate::jsonl::glob_by_mtime(&pattern)?;
    let mut state = load_state();
    let mut emitted = 0;
    let out = out_path();
    for f in &files {
        let project = f
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());
        let repo_root = format!("~/{project}");
        let arr = match read_full_array(f) {
            Ok(a) => a,
            Err(_) => continue,
        };
        for rec in arr {
            let sid = rec
                .get("sessionId")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            if sid.is_empty() {
                continue;
            }
            let mid = rec.get("messageId").and_then(|x| x.as_i64()).unwrap_or(0);
            let ts = rec.get("timestamp").and_then(|x| x.as_str()).unwrap_or("");
            let typ = rec.get("type").and_then(|x| x.as_str()).unwrap_or("");

            let last = *state.seen.get(&sid).unwrap_or(&-1);
            if mid <= last {
                continue;
            }
            if mid == 0 {
                emit_record(&out, "SessionStart", "gemini_shim", &sid, ts, &repo_root, "gemini")?;
                emitted += 1;
            }
            if typ == "user" {
                emit_record(
                    &out,
                    "UserPromptSubmit",
                    "gemini_shim",
                    &sid,
                    ts,
                    &repo_root,
                    "gemini",
                )?;
                emitted += 1;
            }
            state.seen.insert(sid, mid);
        }
    }
    save_state(&state)?;
    Ok(emitted)
}
