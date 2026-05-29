//! Build the dashboard graph response from the relational read-model.
//!
//! Successor to the event-sourced reconstruction (the old
//! "activegraph" replay — deleted, see
//! plans/sentinel-viz-yeet-activegraph.md). Sessions and their hook
//! events are now first-class rows; this module reads them directly
//! instead of replaying a flat event log and re-deriving sessions in
//! memory. No synthetic nodes, no JSONL re-augmentation, no per-session
//! hidden caps, no top-K truncation — session presence is governed by
//! `last_activity_ts`, event depth by the request `limit`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::json;

use crate::awaiting;
use crate::db;
use crate::model::{kind, node_kind, GraphResponse, GraphStats, Node, RecentEvent, SessionStatus};
use crate::transcript;

/// Liveness thresholds (seconds). Match `viz_server.py:574-578`.
const FIRING_THRESHOLD: f64 = 30.0;
const BUSY_THRESHOLD: f64 = 90.0;
const IDLE_THRESHOLD: f64 = 300.0;
const DORMANT_THRESHOLD: f64 = 1800.0;
/// Maximum age (seconds) for a session to remain flagged as
/// `awaiting_user`. 24h so sessions stuck overnight still surface in
/// the STUCK badge; past this they fall to `dead`.
const AWAIT_FRESHNESS_SECS: f64 = 86_400.0;

/// Map a session's last-activity age (seconds) to its base liveness
/// status. Pure and total: the half-open bands tile `[0, ∞)` with no
/// gap or overlap, so a higher age can never decay to a *fresher*
/// status. `AwaitingUser` is layered on top by the caller.
fn classify_liveness(age: f64) -> SessionStatus {
    if age < FIRING_THRESHOLD {
        SessionStatus::Firing
    } else if age < BUSY_THRESHOLD {
        SessionStatus::Busy
    } else if age < IDLE_THRESHOLD {
        SessionStatus::Idle
    } else if age < DORMANT_THRESHOLD {
        SessionStatus::Dormant
    } else {
        SessionStatus::Dead
    }
}

/// Knobs the HTTP layer can override per-request.
#[derive(Debug, Clone)]
pub struct GraphOpts {
    /// Newest-events window: the ticker/sparkline tail size.
    pub limit: usize,
    /// Drop sessions whose `last_activity_ts` is older than this many
    /// seconds. `None` = no floor.
    pub since_secs: Option<i64>,
    /// Retained for API compatibility with the old event-graph; the
    /// relational model has no hook *nodes*, so this is ignored.
    pub include_hooks: bool,
    /// Session id that should be included even if it falls outside the
    /// recent-activity window (drives `?focus=`).
    pub focused_session: Option<String>,
}

impl Default for GraphOpts {
    fn default() -> Self {
        Self {
            limit: 6_000,
            since_secs: Some(6 * 3600),
            include_hooks: false,
            focused_session: None,
        }
    }
}

pub fn load_graph(conn: &Connection, limit: usize) -> Result<GraphResponse> {
    load_graph_with(conn, GraphOpts { limit, ..GraphOpts::default() })
}

/// Read the relational store into a dashboard graph snapshot.
pub fn load_graph_with(conn: &Connection, opts: GraphOpts) -> Result<GraphResponse> {
    let limit = opts.limit;

    // 1. Recent sessions (by last_activity_ts), plus the focused one if
    //    it fell outside the window.
    let mut sessions = db::read_recent_sessions(conn, opts.since_secs, db::MAX_SESSIONS)?;
    if let Some(focus) = opts.focused_session.as_deref() {
        if !sessions.iter().any(|s| s.session_id == focus) {
            if let Some(extra) = db::read_session(conn, focus)? {
                sessions.push(extra);
            }
        }
    }

    let sid_set: std::collections::HashSet<&str> =
        sessions.iter().map(|s| s.session_id.as_str()).collect();

    // 2. Newest `limit` hook events globally (cheap PK-descending walk),
    //    then keep only those for the sessions we're displaying. No
    //    hidden per-session cap; recent-by-activity sessions own the
    //    newest events so the global tail covers them.
    let all_recent = db::read_recent_events(conn, limit)?; // newest-first
    let max_seq = all_recent.first().map(|e| e.id).unwrap_or(0);
    let event_rows: Vec<db::HookEventRow> = all_recent
        .into_iter()
        .filter(|e| sid_set.contains(e.session_id.as_str()))
        .collect();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    // 3. One SentinelSession node per session, with liveness derived
    //    from last_activity_ts (and transcript mtime, which can be
    //    fresher) — no event scan needed.
    let mut nodes: Vec<Node> = Vec::with_capacity(sessions.len());
    for s in &sessions {
        let last_ts = parse_ts_to_epoch(&s.last_activity_ts);
        let tpath = transcript::find_transcript(&s.session_id);
        let tmtime = tpath
            .as_ref()
            .and_then(|p| p.metadata().ok())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let last_activity = last_ts.max(tmtime);
        let age = if last_activity > 0.0 { now - last_activity } else { 1e9 };

        let mut status = classify_liveness(age);
        let mut awaiting_kind = None;
        let mut awaiting_question = None;
        let mut awaiting_options = None;
        if let Some(p) = tpath.as_ref() {
            if tmtime > 0.0 && (now - tmtime) <= AWAIT_FRESHNESS_SECS {
                let (kind_opt, question, options) = awaiting::detect(p);
                if let Some(k) = kind_opt {
                    status = SessionStatus::AwaitingUser;
                    awaiting_kind = Some(k.to_string());
                    awaiting_question = question;
                    awaiting_options = Some(options);
                }
            }
        }

        let node_ts = if s.last_activity_ts.is_empty() {
            s.started_at.clone()
        } else {
            s.last_activity_ts.clone()
        };
        nodes.push(Node {
            id: format!("{}#{}", node_kind::SESSION, s.session_id),
            kind: node_kind::SESSION.to_string(),
            data: json!({
                "session_id": s.session_id,
                "cwd": s.cwd,
                "platform": s.platform,
                "started_at": s.started_at,
                "source_harness": s.source_harness,
            }),
            ts: node_ts,
            seq: 0,
            session_status: Some(status),
            last_activity_age_s: if last_activity > 0.0 { Some(age as i64) } else { None },
            awaiting_kind,
            awaiting_question,
            awaiting_options,
            category: None,
        });
    }

    // 4. Event tail: rows came newest-first; flip to chronological so
    //    the ticker's reverse-loop in JS still renders newest-at-top.
    let events: Vec<RecentEvent> = event_rows
        .iter()
        .rev()
        .map(|e| {
            let k = match e.outcome.as_str() {
                "deny" | "denied" | "block" | "force_stop" => kind::HOOK_DENIED,
                _ => kind::HOOK_INGESTED,
            };
            RecentEvent {
                seq: e.id,
                kind: k.to_string(),
                payload: json!({
                    "session_id": e.session_id,
                    "ts": e.ts,
                    "tool": e.tool,
                    "sentinel_event": e.sentinel_event,
                    "hook": e.hook,
                    "outcome": e.outcome,
                    "duration_us": e.duration_us,
                    "source_harness": e.source_harness,
                }),
                ts: e.ts.clone(),
            }
        })
        .collect();

    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    by_type.insert(node_kind::SESSION.to_string(), nodes.len());

    let stats = GraphStats {
        nodes_total: nodes.len(),
        edges_total: 0,
        by_type: by_type.clone(),
        by_outcome: BTreeMap::new(),
        events_total: events.len(),
        corpus_nodes: nodes.len(),
        corpus_edges: 0,
        corpus_by_type: by_type,
        corpus_by_outcome: BTreeMap::new(),
    };

    Ok(GraphResponse {
        nodes,
        edges: Vec::new(),
        events,
        max_seq,
        window_limit: limit,
        stats,
        error: None,
    })
}

pub fn load_graph_from_default(limit: usize) -> Result<GraphResponse> {
    let path = db::default_db_path()?;
    load_graph_from_path(&path, limit)
}

pub fn load_graph_from_path(path: &Path, limit: usize) -> Result<GraphResponse> {
    if !path.exists() {
        return Ok(GraphResponse {
            nodes: vec![],
            edges: vec![],
            events: vec![],
            max_seq: 0,
            window_limit: limit,
            stats: GraphStats::default(),
            error: Some(format!("db not found: {}", path.display())),
        });
    }
    let conn = db::open_ro(path)?;
    load_graph(&conn, limit)
}

/// Best-effort RFC3339 → epoch seconds. Matches Python's `_ts_to_epoch`:
/// returns 0.0 on parse failure, normalises Z→+00:00, caps frac digits.
pub(crate) fn parse_ts_to_epoch(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let t = s.replace('Z', "+00:00");
    let normalised = normalise_frac(&t);
    chrono::DateTime::parse_from_rfc3339(&normalised)
        .map(|d| d.timestamp() as f64 + (d.timestamp_subsec_micros() as f64) / 1_000_000.0)
        .unwrap_or(0.0)
}

pub(crate) fn normalise_frac_pub(t: &str) -> String {
    normalise_frac(t)
}

fn normalise_frac(t: &str) -> String {
    if let Some(dot) = t.find('.') {
        let (head, rest) = t.split_at(dot);
        let rest = &rest[1..];
        let (frac, tz) = if let Some(p) = rest.find(['+', '-']) {
            (&rest[..p], &rest[p..])
        } else {
            (rest, "")
        };
        let frac6: String = frac.chars().take(6).collect();
        format!("{head}.{frac6}{tz}")
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionStatus;

    /// Pin every age→status boundary. The bands are half-open
    /// `[lo, hi)`, so each threshold value belongs to the *older*
    /// band, not the fresher one.
    #[test]
    fn classify_liveness_bucket_boundaries() {
        assert_eq!(classify_liveness(0.0), SessionStatus::Firing);
        assert_eq!(classify_liveness(29.999), SessionStatus::Firing);
        assert_eq!(classify_liveness(60.0), SessionStatus::Busy);
        assert_eq!(classify_liveness(200.0), SessionStatus::Idle);
        assert_eq!(classify_liveness(1000.0), SessionStatus::Dormant);
        assert_eq!(classify_liveness(5000.0), SessionStatus::Dead);

        assert_eq!(classify_liveness(FIRING_THRESHOLD), SessionStatus::Busy);
        assert_eq!(classify_liveness(BUSY_THRESHOLD), SessionStatus::Idle);
        assert_eq!(classify_liveness(IDLE_THRESHOLD), SessionStatus::Dormant);
        assert_eq!(classify_liveness(DORMANT_THRESHOLD), SessionStatus::Dead);

        assert_eq!(classify_liveness(1e9), SessionStatus::Dead);
    }

    /// Monotonicity: status never gets *fresher* as age grows.
    #[test]
    fn classify_liveness_is_monotonic() {
        let rank = |s: SessionStatus| match s {
            SessionStatus::Firing => 0,
            SessionStatus::Busy => 1,
            SessionStatus::Idle => 2,
            SessionStatus::Dormant => 3,
            SessionStatus::Dead => 4,
            SessionStatus::AwaitingUser => 5,
        };
        let mut prev = -1;
        let mut age = 0.0;
        while age <= DORMANT_THRESHOLD + 100.0 {
            let r = rank(classify_liveness(age));
            assert!(r >= prev, "status got fresher at age {age}");
            prev = r;
            age += 5.0;
        }
    }
}
