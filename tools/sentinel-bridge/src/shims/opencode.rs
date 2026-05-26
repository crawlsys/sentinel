//! opencode_shim — translate the opencode SQLite store
//! (~/.local/share/opencode/opencode.db) into bridge hook-invocation
//! records. Port of harness-shims/opencode_shim.py.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

use crate::shims::{emit_record, ms_to_iso};

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct State {
    session_last_updated: i64,
    message_last_updated: i64,
    part_last_updated: i64,
}

fn db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db")
}

fn out_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".opencode")
        .join("sentinel")
        .join("metrics")
        .join("hook-invocations.jsonl")
}

fn state_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".opencode")
        .join("sentinel")
        .join("metrics")
        .join("opencode-shim.state.json")
}

fn load_state() -> State {
    let path = state_path();
    if let Ok(raw) = fs::read_to_string(&path) {
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
    let db = db_path();
    if !db.exists() {
        return Ok(0);
    }
    let conn = Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;
    let mut state = load_state();
    let mut emitted = 0;
    let out = out_path();

    // 1. New sessions → SessionStart; archived → Stop.
    {
        let mut stmt = conn.prepare(
            "SELECT id, directory, time_created, time_updated, time_archived
             FROM session WHERE time_updated > ? ORDER BY time_updated",
        )?;
        let prior = state.session_last_updated;
        let rows = stmt.query_map([prior], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        })?;
        let mut max_session = state.session_last_updated;
        for row in rows {
            let (sid, dir, t_created, t_updated, t_archived) = row?;
            let repo = dir.unwrap_or_else(|| "/".to_string());
            if t_created > prior {
                emit_record(
                    &out,
                    "SessionStart",
                    "opencode_shim",
                    &sid,
                    &ms_to_iso(t_created),
                    &repo,
                    "opencode",
                )?;
                emitted += 1;
            }
            if let Some(arc) = t_archived {
                if arc > prior {
                    emit_record(
                        &out,
                        "Stop",
                        "opencode_shim",
                        &sid,
                        &ms_to_iso(arc),
                        &repo,
                        "opencode",
                    )?;
                    emitted += 1;
                }
            }
            max_session = max_session.max(t_updated);
        }
        state.session_last_updated = max_session;
    }

    // 2. New messages → UserPromptSubmit for role=user.
    {
        let mut stmt = conn.prepare(
            "SELECT m.id, m.session_id, m.time_created, m.data, s.directory
             FROM message m
             LEFT JOIN session s ON s.id = m.session_id
             WHERE m.time_updated > ? ORDER BY m.time_updated",
        )?;
        let prior = state.message_last_updated;
        let rows = stmt.query_map([prior], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut max_msg = state.message_last_updated;
        for row in rows {
            let (_mid, sid, t_created, data_raw, dir) = row?;
            let parsed: Value = data_raw
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            let role = parsed.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "user" {
                let repo = dir.unwrap_or_else(|| "/".to_string());
                emit_record(
                    &out,
                    "UserPromptSubmit",
                    "opencode_shim",
                    &sid,
                    &ms_to_iso(t_created),
                    &repo,
                    "opencode",
                )?;
                emitted += 1;
            }
            max_msg = max_msg.max(t_created);
        }
        state.message_last_updated = max_msg;
    }

    // 3. New tool parts → PreToolUse / PostToolUse.
    {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.session_id, p.time_created, p.time_updated, p.data, s.directory
             FROM part p
             LEFT JOIN session s ON s.id = p.session_id
             WHERE p.time_updated > ?
               AND p.data LIKE '%\"type\":\"tool\"%'
             ORDER BY p.time_updated",
        )?;
        let prior = state.part_last_updated;
        let rows = stmt.query_map([prior], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut max_part = state.part_last_updated;
        for row in rows {
            let (_pid, sid, t_created, t_updated, data_raw, dir) = row?;
            let parsed: Value = data_raw
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            let tool = parsed
                .get("tool")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            let status = parsed
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(|x| x.as_str())
                .unwrap_or("unknown");
            let repo = dir.unwrap_or_else(|| "/".to_string());

            if t_created > prior {
                emit_record(
                    &out,
                    "PreToolUse",
                    &format!("opencode_shim_tool_{tool}"),
                    &sid,
                    &ms_to_iso(t_created),
                    &repo,
                    "opencode",
                )?;
                emitted += 1;
            }
            if matches!(status, "completed" | "error" | "denied") {
                emit_record(
                    &out,
                    "PostToolUse",
                    &format!("opencode_shim_tool_{tool}"),
                    &sid,
                    &ms_to_iso(t_updated),
                    &repo,
                    "opencode",
                )?;
                emitted += 1;
            }
            max_part = max_part.max(t_updated);
        }
        state.part_last_updated = max_part;
    }

    save_state(&state)?;
    Ok(emitted)
}
