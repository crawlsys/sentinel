use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;

use crate::awaiting;
use crate::db;
use crate::model::{
    kind, node_kind, Edge, GraphResponse, GraphStats, Node, NodeCategory, RecentEvent,
    SessionStatus,
};
use crate::transcript;

/// Window strategy constants. Match `viz_server.py:490-491`.
const K_SESSIONS: usize = 6;
const PER_SESSION_CAP: usize = 40;

/// Liveness thresholds (seconds). Match `viz_server.py:574-578`.
const FIRING_THRESHOLD: f64 = 30.0;
const BUSY_THRESHOLD: f64 = 90.0;
const IDLE_THRESHOLD: f64 = 300.0;
const DORMANT_THRESHOLD: f64 = 1800.0;
/// Maximum age (seconds) for a session to remain flagged as
/// `awaiting_user`. Tuned to 24h so sessions stuck overnight still
/// surface in the STUCK badge — the original 1h Python value was
/// "user walked away, forget it" which loses sight of multi-hour
/// blocked work. Past this window the session falls to `dead`.
const AWAIT_FRESHNESS_SECS: f64 = 86_400.0;

/// Knobs the HTTP layer can override per-request.
#[derive(Debug, Clone, Copy)]
pub struct GraphOpts {
    pub limit: usize,
    /// Drop `sentinel.*` events older than this many seconds. `None`
    /// means no time floor (matches the old behaviour of "last N
    /// events regardless of age").
    pub since_secs: Option<i64>,
    /// When `false` (default), hooks are filtered out and direct
    /// `session → tool_call` edges are synthesised in their place.
    pub include_hooks: bool,
}

impl Default for GraphOpts {
    fn default() -> Self {
        Self { limit: 100, since_secs: Some(6 * 3600), include_hooks: false }
    }
}

pub fn load_graph(conn: &Connection, limit: usize) -> Result<GraphResponse> {
    load_graph_with(conn, GraphOpts { limit, ..GraphOpts::default() })
}

/// Read events into a windowed graph snapshot. Successor to
/// `viz_server.py:load_graph()`.
pub fn load_graph_with(conn: &Connection, opts: GraphOpts) -> Result<GraphResponse> {
    let limit = opts.limit;
    let events = db::read_events(conn)?;

    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut edges_all: Vec<Edge> = Vec::new();
    let mut edge_keys: HashSet<String> = HashSet::new();
    let mut recent_events: Vec<RecentEvent> = Vec::new();
    let mut max_seq: i64 = 0;

    for ev in &events {
        if ev.seq > max_seq {
            max_seq = ev.seq;
        }
        match ev.kind.as_str() {
            kind::OBJECT_CREATED => {
                let Some(obj) = ev.payload.get("object") else { continue };
                let Some(nid) = obj.get("id").and_then(|v| v.as_str()) else { continue };
                let kind_str = obj.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let data = obj.get("data").cloned().unwrap_or(serde_json::Value::Null);
                nodes.insert(
                    nid.to_string(),
                    Node {
                        id: nid.to_string(),
                        kind: kind_str,
                        data,
                        ts: ev.timestamp.clone(),
                        seq: ev.seq,
                        session_status: None,
                        last_activity_age_s: None,
                        awaiting_kind: None,
                        awaiting_question: None,
                        awaiting_options: None,
                        category: None,
                    },
                );
            }
            kind::RELATION_CREATED => {
                let Some(rel) = ev.payload.get("relation") else { continue };
                let Some(src) = rel.get("source").and_then(|v| v.as_str()) else { continue };
                let Some(tgt) = rel.get("target").and_then(|v| v.as_str()) else { continue };
                let rtype = rel.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let key = format!("{src}->{tgt}:{rtype}");
                if edge_keys.insert(key) {
                    edges_all.push(Edge {
                        source: src.to_string(),
                        target: tgt.to_string(),
                        kind: rtype,
                        ts: ev.timestamp.clone(),
                    });
                }
            }
            _ => {}
        }
        if ev.kind.starts_with("sentinel.") {
            recent_events.push(RecentEvent {
                seq: ev.seq,
                kind: ev.kind.clone(),
                payload: ev.payload.clone(),
                ts: ev.timestamp.clone(),
            });
        }
    }

    // Derive `next_in_session` edges between consecutive hook invocations.
    let mut by_session: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in nodes.values() {
        if n.kind == node_kind::HOOK_INVOCATION {
            if let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) {
                by_session.entry(sid.to_string()).or_default().push(n);
            }
        }
    }
    // Derive `next_tool_call` edges between consecutive tool-calls in
    // a session — the "chain" layout the user asks for. Built from
    // SentinelToolCall nodes sorted by ts_sec (sub-second collisions
    // broken by seq).
    let mut tc_by_session: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in nodes.values() {
        if n.kind == node_kind::TOOL_CALL {
            if let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) {
                tc_by_session.entry(sid.to_string()).or_default().push(n);
            }
        }
    }
    for (_sid, mut tcs) in tc_by_session {
        tcs.sort_by_key(|n| {
            let ts = n
                .data
                .get("ts_sec")
                .and_then(|v| v.as_str())
                .or_else(|| n.data.get("ts").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| n.ts.clone());
            (ts, n.seq)
        });
        for w in tcs.windows(2) {
            let (a, b) = (w[0], w[1]);
            let key = format!("{}->{}:next_tool_call", a.id, b.id);
            if edge_keys.insert(key) {
                let ts = b
                    .data
                    .get("ts_sec")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| b.ts.clone());
                edges_all.push(Edge {
                    source: a.id.clone(),
                    target: b.id.clone(),
                    kind: "next_tool_call".to_string(),
                    ts,
                });
            }
        }
    }

    for (_sid, mut invs) in by_session {
        invs.sort_by_key(|n| {
            let ts = n
                .data
                .get("ts")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| n.ts.clone());
            (ts, n.seq)
        });
        for w in invs.windows(2) {
            let (a, b) = (w[0], w[1]);
            let key = format!("{}->{}:next_in_session", a.id, b.id);
            if edge_keys.insert(key) {
                let ts = b
                    .data
                    .get("ts")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| b.ts.clone());
                edges_all.push(Edge {
                    source: a.id.clone(),
                    target: b.id.clone(),
                    kind: "next_in_session".to_string(),
                    ts,
                });
            }
        }
    }

    // Full-corpus stats
    let mut corpus_by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut corpus_by_outcome: BTreeMap<String, usize> = BTreeMap::new();
    for n in nodes.values() {
        *corpus_by_type.entry(n.kind.clone()).or_insert(0) += 1;
        if let Some(outcome) = n.data.get("outcome").and_then(|v| v.as_str()) {
            *corpus_by_outcome.entry(outcome.to_string()).or_insert(0) += 1;
        }
    }

    // Window: K most-recently-active sessions, capped per-session.
    let mut session_max_seq: HashMap<String, i64> = HashMap::new();
    let mut inv_by_session: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in nodes.values() {
        if n.kind != node_kind::HOOK_INVOCATION {
            continue;
        }
        let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) else { continue };
        inv_by_session.entry(sid.to_string()).or_default().push(n);
        let e = session_max_seq.entry(sid.to_string()).or_insert(0);
        if n.seq > *e {
            *e = n.seq;
        }
    }
    let mut sessions_by_recency: Vec<(String, i64)> =
        session_max_seq.iter().map(|(k, v)| (k.clone(), *v)).collect();
    sessions_by_recency.sort_by(|a, b| b.1.cmp(&a.1));
    let top_sids: Vec<String> = sessions_by_recency
        .into_iter()
        .take(K_SESSIONS)
        .map(|(s, _)| s)
        .collect();
    let mut kept_inv_ids: HashSet<String> = HashSet::new();
    for sid in &top_sids {
        if let Some(invs) = inv_by_session.get(sid) {
            let mut sorted: Vec<&&Node> = invs.iter().collect();
            sorted.sort_by_key(|n| std::cmp::Reverse(n.seq));
            for n in sorted.into_iter().take(PER_SESSION_CAP) {
                kept_inv_ids.insert(n.id.clone());
            }
        }
    }
    let mut kept_session_ids: HashSet<String> = HashSet::new();
    for e in &edges_all {
        if kept_inv_ids.contains(&e.target) && e.source.starts_with(node_kind::SESSION) {
            kept_session_ids.insert(e.source.clone());
        }
    }
    let mut kept_ids: HashSet<String> = kept_inv_ids.union(&kept_session_ids).cloned().collect();

    // Per-event timestamp resolution. `recent_events.ts` is the SQL
    // column, which the bridge leaves empty for `sentinel.*` events;
    // the payload always carries `ts_sec` and/or `ts`.
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let event_ts_epoch = |ev: &RecentEvent| -> i64 {
        let payload_ts = ev
            .payload
            .get("ts_sec")
            .and_then(|v| v.as_str())
            .or_else(|| ev.payload.get("ts").and_then(|v| v.as_str()))
            .unwrap_or("");
        if !payload_ts.is_empty() {
            return parse_ts_to_epoch(payload_ts) as i64;
        }
        if !ev.ts.is_empty() {
            return parse_ts_to_epoch(&ev.ts) as i64;
        }
        0
    };

    // Apply optional time floor — drops events older than `since_secs`.
    if let Some(since) = opts.since_secs {
        let cutoff = now_secs - since;
        recent_events.retain(|ev| {
            let t = event_ts_epoch(ev);
            t == 0 || t >= cutoff
        });
    }

    // Mirror the hook-hide policy in the ticker: if the graph isn't
    // showing hook nodes, drop hook-level events too. Tool-call
    // events already carry n_hooks + outcomes, so nothing is lost.
    if !opts.include_hooks {
        recent_events.retain(|ev| {
            ev.kind != kind::HOOK_INGESTED && ev.kind != kind::HOOK_DENIED
        });
    }

    // Compute the visible event tail (matches what the ticker will show).
    let events_limit_for_window = (limit * 6).max(600);
    let events_tail_for_window: &[RecentEvent] = if recent_events.len() > events_limit_for_window {
        &recent_events[recent_events.len() - events_limit_for_window..]
    } else {
        &recent_events[..]
    };

    // Expand kept_ids ONLY for the freshest events the user is likely
    // to click on. Pulling in every node referenced by the full 600-
    // event tail bloats the canvas (each tool_call event has a
    // unique id, so the expansion is unbounded). Cap at 60 — the
    // ticker's first viewport-worth of rows.
    let expand_cap = 60.min(events_tail_for_window.len());
    let expand_from = events_tail_for_window
        .len()
        .saturating_sub(expand_cap);
    for ev in &events_tail_for_window[expand_from..] {
        if let Some(s) = ev.payload.get("session_id").and_then(|v| v.as_str()) {
            for n in nodes.values() {
                if n.kind == node_kind::SESSION
                    && n.data.get("session_id").and_then(|v| v.as_str()) == Some(s)
                {
                    kept_ids.insert(n.id.clone());
                }
            }
        }
        if let Some(tcid) = ev.payload.get("tool_call_id").and_then(|v| v.as_str()) {
            if nodes.contains_key(tcid) {
                kept_ids.insert(tcid.to_string());
            }
        }
        if let Some(hid) = ev.payload.get("invocation_id").and_then(|v| v.as_str()) {
            if nodes.contains_key(hid) {
                kept_ids.insert(hid.to_string());
            }
        }
    }

    let mut kept_nodes: Vec<Node> = nodes
        .values()
        .filter(|n| kept_ids.contains(&n.id))
        .cloned()
        .collect();
    let mut kept_edges: Vec<Edge> = edges_all
        .iter()
        .filter(|e| kept_ids.contains(&e.source) && kept_ids.contains(&e.target))
        .cloned()
        .collect();

    // Annotate tool-call nodes with a coarse category so the UI can
    // colour by intent without inspecting `tool` client-side.
    for n in kept_nodes.iter_mut() {
        if n.kind != node_kind::TOOL_CALL {
            continue;
        }
        let tool = n.data.get("tool").and_then(|v| v.as_str()).unwrap_or("");
        let sev = n.data.get("sentinel_event").and_then(|v| v.as_str());
        n.category = Some(NodeCategory::from_tool(tool, sev));
    }

    // Default-hide hooks: drop SentinelHookInvocation nodes + their
    // edges, then synthesise direct session → tool-call edges by
    // walking the (session → hook → tool-call) chain from the full
    // edge list. This keeps the canvas legible (the bridge produces
    // ~10× as many hooks as tool-calls).
    if !opts.include_hooks {
        // Build the mapping while we still have all the data.
        let mut hook_session: HashMap<&str, &str> = HashMap::new();
        let mut hook_to_tc: HashMap<&str, &str> = HashMap::new();
        for e in &edges_all {
            if e.kind == "has_invocation" && e.target.starts_with(node_kind::HOOK_INVOCATION) {
                hook_session.insert(&e.target, &e.source);
            } else if e.kind == "has_tool_call"
                && e.source.starts_with(node_kind::HOOK_INVOCATION)
                && e.target.starts_with(node_kind::TOOL_CALL)
            {
                hook_to_tc.insert(&e.source, &e.target);
            }
        }
        let mut synth_keys: HashSet<String> = HashSet::new();
        let mut synth_edges: Vec<Edge> = Vec::new();
        for (hook_id, tc_id) in &hook_to_tc {
            if !kept_ids.contains(*tc_id) {
                continue;
            }
            let Some(sid) = hook_session.get(hook_id) else { continue };
            if !kept_ids.contains(*sid) {
                continue;
            }
            let key = format!("{sid}->{tc_id}:has_tool_call_synth");
            if synth_keys.insert(key) {
                synth_edges.push(Edge {
                    source: sid.to_string(),
                    target: tc_id.to_string(),
                    kind: "has_tool_call_synth".to_string(),
                    ts: String::new(),
                });
            }
        }
        kept_nodes.retain(|n| n.kind != node_kind::HOOK_INVOCATION);
        let kept_set: HashSet<String> = kept_nodes.iter().map(|n| n.id.clone()).collect();
        kept_edges.retain(|e| kept_set.contains(&e.source) && kept_set.contains(&e.target));
        kept_edges.extend(synth_edges);
    }

    // Window stats
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_outcome: BTreeMap<String, usize> = BTreeMap::new();
    for n in &kept_nodes {
        *by_type.entry(n.kind.clone()).or_insert(0) += 1;
        if let Some(outcome) = n.data.get("outcome").and_then(|v| v.as_str()) {
            *by_outcome.entry(outcome.to_string()).or_insert(0) += 1;
        }
    }

    // Per-session liveness annotation.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let mut max_hook_ts_by_sid: HashMap<String, f64> = HashMap::new();
    for n in nodes.values() {
        if n.kind != node_kind::HOOK_INVOCATION {
            continue;
        }
        let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) else { continue };
        let ts_str = n.data.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let ts = parse_ts_to_epoch(ts_str);
        let e = max_hook_ts_by_sid.entry(sid.to_string()).or_insert(0.0);
        if ts > *e {
            *e = ts;
        }
    }

    for n in kept_nodes.iter_mut() {
        if n.kind != node_kind::SESSION {
            continue;
        }
        let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()).map(|s| s.to_string())
        else {
            continue;
        };
        let last_hook = max_hook_ts_by_sid.get(&sid).copied().unwrap_or(0.0);
        let tpath = transcript::find_transcript(&sid);
        let tmtime = tpath
            .as_ref()
            .and_then(|p| p.metadata().ok())
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let last_activity = last_hook.max(tmtime);
        let age = if last_activity > 0.0 { now - last_activity } else { 1e9 };
        let status = if age < FIRING_THRESHOLD {
            SessionStatus::Firing
        } else if age < BUSY_THRESHOLD {
            SessionStatus::Busy
        } else if age < IDLE_THRESHOLD {
            SessionStatus::Idle
        } else if age < DORMANT_THRESHOLD {
            SessionStatus::Dormant
        } else {
            SessionStatus::Dead
        };

        let mut final_status = status;
        if let Some(p) = tpath.as_ref() {
            if tmtime > 0.0 && (now - tmtime) <= AWAIT_FRESHNESS_SECS {
                let (kind_opt, question, options) = awaiting::detect(p);
                if let Some(k) = kind_opt {
                    final_status = SessionStatus::AwaitingUser;
                    n.awaiting_kind = Some(k.to_string());
                    n.awaiting_question = question;
                    n.awaiting_options = Some(options);
                }
            }
        }
        n.session_status = Some(final_status);
        n.last_activity_age_s = if last_activity > 0.0 { Some(age as i64) } else { None };
    }

    // Ticker tail
    let events_limit = (limit * 6).max(600);
    let events_tail = if recent_events.len() > events_limit {
        recent_events[recent_events.len() - events_limit..].to_vec()
    } else {
        recent_events
    };

    let stats = GraphStats {
        nodes_total: kept_nodes.len(),
        edges_total: kept_edges.len(),
        by_type,
        by_outcome,
        events_total: events_tail.len(),
        corpus_nodes: nodes.len(),
        corpus_edges: edges_all.len(),
        corpus_by_type,
        corpus_by_outcome,
    };

    Ok(GraphResponse {
        nodes: kept_nodes,
        edges: kept_edges,
        events: events_tail,
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
