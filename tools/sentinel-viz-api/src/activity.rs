use std::collections::HashMap;

use serde_json::Value;

use crate::model::{ActivityEvent, ActivityResponse, Segment, ToolCallSummary};
use crate::transcript::{self, trim};

/// Port of `session_activity()` in viz_server.py.
pub fn session_activity(
    session_id: &str,
    limit: usize,
    at_ts: Option<&str>,
    window_secs: i64,
) -> ActivityResponse {
    let path = transcript::find_transcript(session_id);
    let Some(path) = path else {
        return ActivityResponse {
            session_id: session_id.to_string(),
            transcript: None,
            events: vec![],
            segments: vec![],
            at_ts: at_ts.map(|s| s.to_string()),
            ..Default::default()
        };
    };

    let Ok(file) = std::fs::read_to_string(&path) else {
        return ActivityResponse {
            session_id: session_id.to_string(),
            transcript: Some(path.display().to_string()),
            events: vec![],
            segments: vec![],
            at_ts: at_ts.map(|s| s.to_string()),
            error: Some("read error".to_string()),
            ..Default::default()
        };
    };

    let mut out: Vec<ActivityEvent> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();
    let mut tool_use_to_seg: HashMap<String, usize> = HashMap::new();
    let mut tool_use_to_tc: HashMap<String, (usize, usize)> = HashMap::new(); // (seg idx, tool_call idx)

    for line in file.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r): Result<Value, _> = serde_json::from_str(line) else { continue };
        let ts = r.get("timestamp").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let typ = r.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let msg = r.get("message").cloned().unwrap_or(Value::Null);

        match typ {
            "user" => handle_user(&msg, &ts, &mut out, &mut segments, &tool_use_to_seg, &mut tool_use_to_tc),
            "assistant" => handle_assistant(&msg, &ts, &mut out, &mut segments, &mut tool_use_to_seg, &mut tool_use_to_tc),
            _ => {}
        }
    }

    if let Some(anchor_str) = at_ts {
        if let Some(anchor) = parse_dt(anchor_str) {
            let filtered_events: Vec<ActivityEvent> = out
                .iter()
                .filter(|e| {
                    if let Some(pt) = parse_dt(&e.ts) {
                        (pt - anchor).num_seconds().abs() <= window_secs
                    } else {
                        false
                    }
                })
                .cloned()
                .collect();
            let filtered_segs: Vec<Segment> = segments
                .iter()
                .filter(|s| {
                    let Some(pt) = parse_dt(&s.ts) else { return false };
                    let pt_end = s
                        .ts_end
                        .as_deref()
                        .and_then(parse_dt)
                        .unwrap_or(pt);
                    (pt - anchor).num_seconds() <= window_secs
                        && (anchor - pt_end).num_seconds() <= window_secs
                })
                .cloned()
                .collect();
            return ActivityResponse {
                session_id: session_id.to_string(),
                transcript: Some(path.display().to_string()),
                total: Some(out.len()),
                total_segments: Some(segments.len()),
                events: filtered_events,
                segments: filtered_segs,
                at_ts: Some(anchor_str.to_string()),
                window_secs: Some(window_secs),
                ..Default::default()
            };
        }
    }

    fn tail<T>(v: Vec<T>, n: usize) -> Vec<T> {
        let len = v.len();
        if len > n {
            v.into_iter().skip(len - n).collect()
        } else {
            v
        }
    }
    let total = out.len();
    let total_segs = segments.len();
    ActivityResponse {
        session_id: session_id.to_string(),
        transcript: Some(path.display().to_string()),
        events: tail(out, limit),
        segments: tail(segments, limit),
        total: Some(total),
        total_segments: Some(total_segs),
        at_ts: at_ts.map(|s| s.to_string()),
        ..Default::default()
    }
}

fn handle_user(
    msg: &Value,
    ts: &str,
    out: &mut Vec<ActivityEvent>,
    segments: &mut Vec<Segment>,
    tool_use_to_seg: &HashMap<String, usize>,
    tool_use_to_tc: &mut HashMap<String, (usize, usize)>,
) {
    let Some(content) = msg.get("content") else { return };
    if let Some(text) = content.as_str() {
        if text.starts_with("<local-command-caveat>")
            || text.starts_with("<system-reminder>")
            || text.starts_with("Caveat:")
        {
            return;
        }
        out.push(ActivityEvent {
            ts: ts.to_string(),
            kind: "user".to_string(),
            text: Some(trim(text, 280)),
            ..Default::default()
        });
        segments.push(Segment {
            ts: ts.to_string(),
            kind: "user_input".to_string(),
            label: "user input".to_string(),
            preview: trim(text, 220),
            ..Default::default()
        });
        return;
    }
    let Some(blocks) = content.as_array() else { return };
    for c in blocks {
        if c.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
            continue;
        }
        let tu_id = c.get("tool_use_id").and_then(|v| v.as_str()).map(|s| s.to_string());
        let result = c.get("content");
        let mut result_text = String::new();
        if let Some(arr) = result.and_then(|v| v.as_array()) {
            for sub in arr {
                if sub.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = sub.get("text").and_then(|v| v.as_str()) {
                        result_text.push_str(t);
                    }
                }
            }
        } else if let Some(s) = result.and_then(|v| v.as_str()) {
            result_text = s.to_string();
        }
        let is_error = c.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
        if !result_text.is_empty() {
            out.push(ActivityEvent {
                ts: ts.to_string(),
                kind: "tool_result".to_string(),
                text: Some(trim(&result_text, 200)),
                is_error: Some(is_error),
                ..Default::default()
            });
        }
        if let Some(tu_id) = tu_id {
            if let Some(&(seg_idx, tc_idx)) = tool_use_to_tc.get(&tu_id) {
                if let Some(seg) = segments.get_mut(seg_idx) {
                    if let Some(tc) = seg.tool_calls.get_mut(tc_idx) {
                        tc.result_preview = Some(trim(&result_text, 180));
                        tc.result_ts = Some(ts.to_string());
                        tc.error = Some(is_error);
                    }
                    seg.ts_end = Some(ts.to_string());
                    if is_error {
                        seg.had_error = true;
                    }
                }
            }
            let _ = tool_use_to_seg;
        }
    }
}

fn handle_assistant(
    msg: &Value,
    ts: &str,
    out: &mut Vec<ActivityEvent>,
    segments: &mut Vec<Segment>,
    tool_use_to_seg: &mut HashMap<String, usize>,
    tool_use_to_tc: &mut HashMap<String, (usize, usize)>,
) {
    let blocks = msg.get("content").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    let mut seg = Segment {
        ts: ts.to_string(),
        ts_end: Some(ts.to_string()),
        kind: "assistant_turn".to_string(),
        ..Default::default()
    };
    let mut accumulated_text = String::new();
    for c in &blocks {
        let t = c.get("type").and_then(|v| v.as_str());
        if t == Some("text") {
            if let Some(txt) = c.get("text").and_then(|v| v.as_str()) {
                let txt = txt.trim();
                if !txt.is_empty() {
                    out.push(ActivityEvent {
                        ts: ts.to_string(),
                        kind: "assistant".to_string(),
                        text: Some(trim(txt, 280)),
                        ..Default::default()
                    });
                    if !accumulated_text.is_empty() {
                        accumulated_text.push(' ');
                    }
                    accumulated_text.push_str(txt);
                }
            }
        } else if t == Some("tool_use") {
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let inp = c.get("input").cloned().unwrap_or(Value::Null);
            let summary = tool_summary(&name, &inp);
            out.push(ActivityEvent {
                ts: ts.to_string(),
                kind: "tool_use".to_string(),
                tool: Some(name.clone()),
                text: Some(summary.clone()),
                ..Default::default()
            });
            let tu_id = c.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let tc = ToolCallSummary {
                id: tu_id.clone(),
                tool: name.clone(),
                summary,
                ..Default::default()
            };
            seg.tools.push(name);
            let tc_idx = seg.tool_calls.len();
            seg.tool_calls.push(tc);
            seg.tool_count += 1;
            if !tu_id.is_empty() {
                tool_use_to_seg.insert(tu_id.clone(), segments.len());
                tool_use_to_tc.insert(tu_id, (segments.len(), tc_idx));
            }
        }
    }
    // Label: chronological dedup, "Nx tool" formatting
    if !seg.tools.is_empty() {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for t in &seg.tools {
            *counts.entry(t.clone()).or_insert(0) += 1;
        }
        let mut seen = std::collections::HashSet::new();
        let mut parts: Vec<String> = Vec::new();
        for t in &seg.tools {
            if !seen.insert(t.clone()) {
                continue;
            }
            let n = counts.get(t).copied().unwrap_or(1);
            if n > 1 {
                parts.push(format!("{n}× {t}"));
            } else {
                parts.push(t.clone());
            }
        }
        seg.label = parts.join(", ");
    } else {
        seg.label = "assistant text".to_string();
    }
    seg.preview = if !accumulated_text.is_empty() {
        trim(&accumulated_text, 220)
    } else if let Some(tc) = seg.tool_calls.first() {
        tc.summary.clone()
    } else {
        String::new()
    };
    seg.text = if accumulated_text.is_empty() {
        None
    } else {
        Some(trim(&accumulated_text, 600))
    };
    segments.push(seg);
}

/// Mirrors `_tool_summary()` in viz_server.py — pure heuristics.
pub fn tool_summary(name: &str, inp: &Value) -> String {
    let dict = inp.as_object();
    let s = |k: &str| -> String {
        dict.and_then(|m| m.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };
    match name {
        "Bash" => trim(&s("command"), 200),
        "Read" | "Write" => trim(&s("file_path"), 200),
        "Edit" => format!(
            "{}  →  {}",
            trim(&s("file_path"), 80),
            trim(&{
                let ns = s("new_string");
                ns.chars().take(80).collect::<String>()
            }, 80)
        ),
        "Grep" => format!(
            "grep '{}' {}",
            trim(&s("pattern"), 60),
            trim(
                &if !s("path").is_empty() { s("path") } else { s("glob") },
                80
            )
        ),
        "Glob" => trim(&s("pattern"), 200),
        "TaskCreate" | "TaskUpdate" => {
            let filtered: serde_json::Map<String, Value> = dict
                .map(|m| m.iter().filter(|(k, _)| k.as_str() != "metadata").map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            trim(&serde_json::to_string(&filtered).unwrap_or_default(), 200)
        }
        "WebFetch" => trim(&s("url"), 200),
        "WebSearch" => trim(&s("query"), 200),
        "AskUserQuestion" => {
            let qs = dict.and_then(|m| m.get("questions")).and_then(|v| v.as_array());
            if let Some(qs) = qs {
                if let Some(q0) = qs.first().and_then(|v| v.as_object()) {
                    if let Some(q) = q0.get("question").and_then(|v| v.as_str()) {
                        return trim(q, 200);
                    }
                }
            }
            String::new()
        }
        "Agent" => trim(&format!("{} · {}", s("description"), s("subagent_type")), 200),
        _ => trim(&serde_json::to_string(inp).unwrap_or_default(), 200),
    }
}

fn parse_dt(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    if s.is_empty() {
        return None;
    }
    let t = s.replace('Z', "+00:00");
    let normalised = crate::graph::normalise_frac_pub(&t);
    chrono::DateTime::parse_from_rfc3339(&normalised).ok()
}
