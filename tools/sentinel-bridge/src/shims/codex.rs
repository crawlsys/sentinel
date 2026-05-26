//! codex_shim — translate ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
//! into bridge hook-invocation records. Port of harness-shims/codex_shim.py.

use anyhow::Result;
use regex::Regex;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::jsonl::{read_new, OffsetState};
use crate::shims::normalize_tool;

static ROLLOUT_RE: OnceLock<Regex> = OnceLock::new();

fn re() -> &'static Regex {
    ROLLOUT_RE.get_or_init(|| Regex::new(r"rollout-[\d\-T]+-([0-9a-f-]{36})\.jsonl$").unwrap())
}

fn out_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("sentinel")
        .join("metrics")
        .join("hook-invocations.jsonl")
}

fn state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".codex")
        .join("sentinel")
        .join("metrics")
        .join("codex-shim.state.json")
}

fn sessions_root() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".codex").join("sessions")
}

pub fn run_once() -> Result<usize> {
    run_inner()
}

fn run_inner() -> Result<usize> {
    let root = sessions_root();
    let pattern = format!("{}/**/rollout-*.jsonl", root.display());
    let files = crate::jsonl::glob_by_mtime(&pattern)?;
    let mut state = OffsetState::load(&state_path())?;
    let mut emitted = 0;
    for f in &files {
        let Some(sid) = session_id_from(f) else {
            continue;
        };
        let key = f.to_string_lossy().into_owned();
        let off = state.offsets.get(&key).copied().unwrap_or(0);
        let (records, new_off) = read_new(f, off)?;
        let mut repo_root = "/".to_string();
        for rec in records {
            for translated in translate(&rec, &sid, &mut repo_root) {
                let (event, hook, ts, root, tool) = translated;
                crate::shims::emit_record(
                    &out_path(),
                    &event,
                    &hook,
                    &sid,
                    &ts,
                    &root,
                    "codex",
                    &tool,
                )?;
                emitted += 1;
            }
        }
        state.offsets.insert(key, new_off);
    }
    state.save(&state_path())?;
    Ok(emitted)
}

fn session_id_from(path: &std::path::Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    re().captures(&name).map(|c| c[1].to_string())
}

/// Translate one rollout line into zero or more hook-invocation
/// records: (event, hook, ts, repo_root, tool). `repo_root` is
/// mutated on session_meta so subsequent records inherit the
/// working dir. `tool` is the Claude-normalized tool name (empty
/// when the event isn't a tool call).
fn translate(
    line: &Value,
    _session_id: &str,
    repo_root: &mut String,
) -> Vec<(String, String, String, String, String)> {
    let typ = line.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let ts = line
        .get("timestamp")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let payload = line.get("payload");

    let mut out = vec![];
    match typ {
        "session_meta" => {
            if let Some(p) = payload {
                if let Some(cwd) = p.get("cwd").and_then(|x| x.as_str()) {
                    *repo_root = cwd.to_string();
                }
            }
            out.push((
                "SessionStart".into(),
                "codex_shim".into(),
                ts,
                repo_root.clone(),
                String::new(),
            ));
        }
        "event_msg" => {
            if let Some(p) = payload {
                let sub = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match sub {
                    "user_message" => out.push((
                        "UserPromptSubmit".into(),
                        "codex_shim".into(),
                        ts,
                        repo_root.clone(),
                        String::new(),
                    )),
                    "task_complete" => out.push((
                        "Stop".into(),
                        "codex_shim".into(),
                        ts,
                        repo_root.clone(),
                        String::new(),
                    )),
                    _ => {}
                }
            }
        }
        "response_item" => {
            if let Some(p) = payload {
                let sub = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match sub {
                    "function_call" => {
                        let raw_tool = p.get("name").and_then(|x| x.as_str()).unwrap_or("exec");
                        let tool = normalize_tool("codex", raw_tool);
                        out.push((
                            "PreToolUse".into(),
                            format!("codex_shim_tool_{raw_tool}"),
                            ts,
                            repo_root.clone(),
                            tool,
                        ));
                    }
                    "function_call_output" => {
                        out.push((
                            "PostToolUse".into(),
                            "codex_shim_tool_result".into(),
                            ts,
                            repo_root.clone(),
                            String::new(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}
