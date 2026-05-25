//! Lightweight at-a-glance metrics for the KPI cards in the UI.
//! Derived entirely from the cached graph snapshot + per-session
//! transcripts already on disk — no new event source.
//!
//! WORKSTREAM: sentinel-viz — internal aggregation; reads cross
//! into claude-code transcripts via `transcript::find_transcript`.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::graph;
use crate::model::{GraphResponse, RecentEvent};
use crate::transcript;

#[derive(Debug, Clone, Serialize)]
pub struct Kpis {
    /// Sessions whose status is firing / busy / awaiting_user.
    pub sessions_active: usize,
    /// Total sessions in the current window.
    pub sessions_total: usize,
    /// Count of ticker events with payload.ts (or ev.ts) within 5min of now.
    pub events_5m: usize,
    /// Events per minute over the last 60s window (float, 2 decimals).
    pub events_per_min: f64,
    /// Sum of token usage across the top-K sessions' transcripts in
    /// the last 5min. None when no usage data is parseable.
    pub tokens_5m: Option<TokenUsage>,
    /// Approximate USD cost — currently None unless we have a real
    /// rate table. Reserved for the next pass.
    pub usd_5m: Option<f64>,
    /// Stuck-session count surfaced from the graph snapshot.
    pub stuck_count: usize,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TokenUsage {
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
}

pub fn compute(graph: &GraphResponse) -> Kpis {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let active_set = ["firing", "busy", "awaiting_user"];
    let mut sessions_active = 0;
    let mut sessions_total = 0;
    let mut stuck = 0;
    let mut session_ids: Vec<String> = Vec::new();
    for n in &graph.nodes {
        if n.kind != "SentinelSession" {
            continue;
        }
        sessions_total += 1;
        if let Some(status) = n.session_status.as_ref().map(|s| serde_json::to_string(s).unwrap_or_default()) {
            let s = status.trim_matches('"');
            if active_set.contains(&s) {
                sessions_active += 1;
            }
            if s == "awaiting_user" {
                let age = n.last_activity_age_s.unwrap_or(0);
                if age > 900 {
                    stuck += 1;
                }
            }
        }
        if let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) {
            session_ids.push(sid.to_string());
        }
    }

    let (events_5m, events_per_min) = count_events_recency(&graph.events, now_secs);

    let tokens_5m = collect_tokens(&session_ids, now_secs);

    Kpis {
        sessions_active,
        sessions_total,
        events_5m,
        events_per_min,
        tokens_5m,
        usd_5m: None,
        stuck_count: stuck,
    }
}

fn count_events_recency(events: &[RecentEvent], now_secs: i64) -> (usize, f64) {
    let mut count_5m: usize = 0;
    let mut count_1m: usize = 0;
    for ev in events {
        let ts = ev
            .payload
            .get("ts_sec")
            .and_then(|v| v.as_str())
            .or_else(|| ev.payload.get("ts").and_then(|v| v.as_str()))
            .unwrap_or("");
        let t = if !ts.is_empty() {
            graph::parse_ts_to_epoch(ts) as i64
        } else if !ev.ts.is_empty() {
            graph::parse_ts_to_epoch(&ev.ts) as i64
        } else {
            0
        };
        if t == 0 {
            continue;
        }
        let age = now_secs - t;
        if age <= 300 {
            count_5m += 1;
        }
        if age <= 60 {
            count_1m += 1;
        }
    }
    let per_min = count_1m as f64;
    (count_5m, (per_min * 100.0).round() / 100.0)
}

/// Parse each visible session's transcript JSONL and tally token
/// usage on `assistant` messages produced in the last 5min. Returns
/// None if no transcript was readable or no usage rows landed in the
/// window.
fn collect_tokens(session_ids: &[String], now_secs: i64) -> Option<TokenUsage> {
    let mut total = TokenUsage::default();
    let mut hits = 0u32;
    let mut seen_sid: HashMap<&str, ()> = HashMap::new();
    for sid in session_ids {
        if seen_sid.insert(sid.as_str(), ()).is_some() {
            continue;
        }
        let Some(path) = transcript::find_transcript(sid) else { continue };
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        for line in content.lines() {
            let Ok(v): Result<serde_json::Value, _> = serde_json::from_str(line.trim()) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let ts = v.get("timestamp").and_then(|t| t.as_str()).unwrap_or("");
            let t = graph::parse_ts_to_epoch(ts) as i64;
            if t == 0 || (now_secs - t) > 300 {
                continue;
            }
            let usage = v.get("message").and_then(|m| m.get("usage"));
            let Some(u) = usage else { continue };
            let g = |k: &str| -> u64 {
                u.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
            };
            total.input += g("input_tokens");
            total.cache_creation += g("cache_creation_input_tokens");
            total.cache_read += g("cache_read_input_tokens");
            total.output += g("output_tokens");
            hits += 1;
        }
    }
    if hits == 0 { None } else { Some(total) }
}
