//! qwen_shim — translate ~/.qwen/projects/*/chats/*.jsonl into bridge
//! hook-invocation records. Port of harness-shims/qwen_shim.py.

use anyhow::Result;
use regex::Regex;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::jsonl::{read_new, OffsetState};
use crate::shims::emit_record;

static CHAT_RE: OnceLock<Regex> = OnceLock::new();

fn re() -> &'static Regex {
    CHAT_RE.get_or_init(|| Regex::new(r"^([0-9a-f-]{36})\.jsonl$").unwrap())
}

fn out_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".qwen")
        .join("sentinel")
        .join("metrics")
        .join("hook-invocations.jsonl")
}

fn state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".qwen")
        .join("sentinel")
        .join("metrics")
        .join("qwen-shim.state.json")
}

fn projects_root() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".qwen").join("projects")
}

pub fn run_once() -> Result<usize> {
    let pattern = format!("{}/**/chats/*.jsonl", projects_root().display());
    let files = crate::jsonl::glob_by_mtime(&pattern)?;
    let mut state = OffsetState::load(&state_path())?;
    let mut emitted = 0;
    for f in &files {
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(sid) = re().captures(&name).map(|c| c[1].to_string()) else {
            continue;
        };
        let key = f.to_string_lossy().into_owned();
        let off = state.offsets.get(&key).copied().unwrap_or(0);
        if off == 0 {
            // First read for this file — emit SessionStart marker so
            // the session shows up even before the first user message.
            emit_record(
                &out_path(),
                "SessionStart",
                "qwen_shim",
                &sid,
                "",
                "/",
                "qwen",
            )?;
            emitted += 1;
        }
        let (records, new_off) = read_new(f, off)?;
        for rec in records {
            for translated in translate(&rec) {
                let (event, hook, ts, root) = translated;
                emit_record(&out_path(), &event, &hook, &sid, &ts, &root, "qwen")?;
                emitted += 1;
            }
        }
        state.offsets.insert(key, new_off);
    }
    state.save(&state_path())?;
    Ok(emitted)
}

/// (event, hook, ts, repo_root) per qwen rollout line.
fn translate(line: &Value) -> Vec<(String, String, String, String)> {
    let typ = line.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let ts = line.get("timestamp").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let cwd = line.get("cwd").and_then(|x| x.as_str()).unwrap_or("/").to_string();
    let mut out = vec![];
    match typ {
        "user" => out.push(("UserPromptSubmit".into(), "qwen_shim".into(), ts, cwd)),
        "assistant" => {
            if let Some(parts) = line
                .get("message")
                .and_then(|m| m.get("parts"))
                .and_then(|p| p.as_array())
            {
                for p in parts {
                    if let Some(fc) = p.get("functionCall") {
                        let tool = fc.get("name").and_then(|x| x.as_str()).unwrap_or("unknown");
                        out.push((
                            "PreToolUse".into(),
                            format!("qwen_shim_tool_{tool}"),
                            ts.clone(),
                            cwd.clone(),
                        ));
                    }
                }
            }
        }
        "tool_result" => {
            if let Some(parts) = line
                .get("message")
                .and_then(|m| m.get("parts"))
                .and_then(|p| p.as_array())
            {
                for p in parts {
                    if let Some(fr) = p.get("functionResponse") {
                        let tool = fr.get("name").and_then(|x| x.as_str()).unwrap_or("unknown");
                        out.push((
                            "PostToolUse".into(),
                            format!("qwen_shim_tool_{tool}"),
                            ts.clone(),
                            cwd.clone(),
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    out
}
