use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;

use crate::awaiting;
use crate::db;
use crate::model::{
    kind, node_kind, Edge, Event, GraphResponse, GraphStats, Node, NodeCategory, RecentEvent,
    SessionStatus,
};
use crate::transcript;

/// Window strategy constants. Per user feedback 2026-05-25:
/// "support up to 5 concurrent sessions, 20 most-recent events from
/// each aggregated into active view (100 datapoints total)."
const K_SESSIONS: usize = 5;
const PER_SESSION_CAP_DEFAULT: usize = 20;
const PER_SESSION_CAP_FOCUSED: usize = 36;

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
#[derive(Debug, Clone)]
pub struct GraphOpts {
    pub limit: usize,
    /// Drop `sentinel.*` events older than this many seconds. `None`
    /// means no time floor (matches the old behaviour of "last N
    /// events regardless of age").
    pub since_secs: Option<i64>,
    /// When `false` (default), hooks are filtered out and direct
    /// `session → tool_call` edges are synthesised in their place.
    pub include_hooks: bool,
    /// Session id (the `data.session_id` value) that should get the
    /// larger `PER_SESSION_CAP_FOCUSED` window. Others get the
    /// default cap.
    pub focused_session: Option<String>,
}

impl Default for GraphOpts {
    fn default() -> Self {
        Self {
            limit: 100,
            since_secs: Some(6 * 3600),
            include_hooks: false,
            focused_session: None,
        }
    }
}

pub fn load_graph(conn: &Connection, limit: usize) -> Result<GraphResponse> {
    load_graph_with(
        conn,
        GraphOpts { limit, ..GraphOpts::default() },
    )
}

/// Read events into a windowed graph snapshot. Successor to
/// `viz_server.py:load_graph()`.
///
/// The pipeline is a sequence of behavior-preserving stages, each
/// extracted into a named helper below: ingest raw events, collapse
/// resumed-session duplicates, derive chain edges, pick the visible
/// window, expand it from the ticker tail, then annotate and stat the
/// kept sub-graph.
pub fn load_graph_with(conn: &Connection, opts: GraphOpts) -> Result<GraphResponse> {
    let limit = opts.limit;
    let events = db::read_events(conn)?;

    let (mut nodes, mut edges_all, mut edge_keys, mut recent_events, max_seq) =
        ingest_events(&events);

    collapse_duplicate_sessions(&mut nodes, &mut edges_all);
    derive_chain_edges(&nodes, &mut edges_all, &mut edge_keys);

    let (corpus_by_type, corpus_by_outcome) = corpus_stats(&nodes);

    let (mut kept_ids, top_sids) = select_window(&nodes, &edges_all, &opts);

    expand_window_from_ticker(
        &nodes,
        &mut recent_events,
        &opts,
        limit,
        &top_sids,
        &mut kept_ids,
    );

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

    annotate_categories(&mut kept_nodes);

    if !opts.include_hooks {
        collapse_hooks_into_synth_edges(&mut kept_nodes, &mut kept_edges, &edges_all, &kept_ids);
    }

    let (by_type, by_outcome) = window_stats(&kept_nodes);

    annotate_liveness(&mut kept_nodes, &nodes);

    let events_tail = build_ticker_tail(recent_events);

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

/// Stage 1 — fold raw events into the node map, deduped edge list,
/// `sentinel.*` recent-event log, and the running `MAX(seq)`.
fn ingest_events(
    events: &[Event],
) -> (
    HashMap<String, Node>,
    Vec<Edge>,
    HashSet<String>,
    Vec<RecentEvent>,
    i64,
) {
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut edges_all: Vec<Edge> = Vec::new();
    let mut edge_keys: HashSet<String> = HashSet::new();
    let mut recent_events: Vec<RecentEvent> = Vec::new();
    let mut max_seq: i64 = 0;

    for ev in events {
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

    (nodes, edges_all, edge_keys, recent_events, max_seq)
}

/// Stage 2 — collapse duplicate `SentinelSession` nodes. The bridge
/// sometimes materialises a new `SentinelSession#<seq>` when a session
/// is resumed, leaving two nodes for the same `session_id`. Keep the
/// highest-seq (most recent) node and rewrite all edges pointing at the
/// dropped duplicates so the topology stays connected.
fn collapse_duplicate_sessions(nodes: &mut HashMap<String, Node>, edges_all: &mut Vec<Edge>) {
    let mut sid_to_winner: HashMap<String, String> = HashMap::new();
    let mut alias_to_winner: HashMap<String, String> = HashMap::new();
    for n in nodes.values() {
        if n.kind != node_kind::SESSION {
            continue;
        }
        let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) else { continue };
        if sid.is_empty() {
            continue;
        }
        match sid_to_winner.get(sid) {
            None => {
                sid_to_winner.insert(sid.to_string(), n.id.clone());
            }
            Some(cur) => {
                let cur_seq = nodes.get(cur).map_or(0, |x| x.seq);
                if n.seq > cur_seq {
                    alias_to_winner.insert(cur.clone(), n.id.clone());
                    sid_to_winner.insert(sid.to_string(), n.id.clone());
                } else {
                    alias_to_winner.insert(n.id.clone(), cur.clone());
                }
            }
        }
    }
    // Drop the loser nodes from the materialisation set.
    for loser in alias_to_winner.keys() {
        nodes.remove(loser);
    }
    // Rewrite edges. If an edge now points at a removed node, swap it to
    // the winner's id. Dedupe via a local seen-set.
    if !alias_to_winner.is_empty() {
        let resolve = |id: &str| -> String {
            alias_to_winner.get(id).cloned().unwrap_or_else(|| id.to_string())
        };
        let mut rewritten: Vec<Edge> = Vec::with_capacity(edges_all.len());
        let mut seen: HashSet<String> = HashSet::new();
        for e in edges_all.drain(..) {
            let s = resolve(&e.source);
            let t = resolve(&e.target);
            if s == t {
                continue;
            }
            let key = format!("{s}->{t}:{}", e.kind);
            if seen.insert(key) {
                rewritten.push(Edge {
                    source: s,
                    target: t,
                    kind: e.kind,
                    ts: e.ts,
                });
            }
        }
        *edges_all = rewritten;
    }
}

/// Stage 3 — synthesise the per-session "chain" edges the layout wants:
/// `next_tool_call` between consecutive tool-calls, then
/// `next_in_session` between consecutive hook invocations. Both sort by
/// `ts_sec`/`ts` with `seq` breaking sub-second ties.
fn derive_chain_edges(
    nodes: &HashMap<String, Node>,
    edges_all: &mut Vec<Edge>,
    edge_keys: &mut HashSet<String>,
) {
    // Tool-call chain (`next_tool_call`).
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
                .or_else(|| n.data.get("ts").and_then(|v| v.as_str())).map_or_else(|| n.ts.clone(), std::string::ToString::to_string);
            (ts, n.seq)
        });
        for w in tcs.windows(2) {
            let (a, b) = (w[0], w[1]);
            let key = format!("{}->{}:next_tool_call", a.id, b.id);
            if edge_keys.insert(key) {
                let ts = b
                    .data
                    .get("ts_sec")
                    .and_then(|v| v.as_str()).map_or_else(|| b.ts.clone(), std::string::ToString::to_string);
                edges_all.push(Edge {
                    source: a.id.clone(),
                    target: b.id.clone(),
                    kind: "next_tool_call".to_string(),
                    ts,
                });
            }
        }
    }

    // Hook chain (`next_in_session`).
    let mut by_session: HashMap<String, Vec<&Node>> = HashMap::new();
    for n in nodes.values() {
        if n.kind == node_kind::HOOK_INVOCATION {
            if let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()) {
                by_session.entry(sid.to_string()).or_default().push(n);
            }
        }
    }
    for (_sid, mut invs) in by_session {
        invs.sort_by_key(|n| {
            let ts = n
                .data
                .get("ts")
                .and_then(|v| v.as_str()).map_or_else(|| n.ts.clone(), std::string::ToString::to_string);
            (ts, n.seq)
        });
        for w in invs.windows(2) {
            let (a, b) = (w[0], w[1]);
            let key = format!("{}->{}:next_in_session", a.id, b.id);
            if edge_keys.insert(key) {
                let ts = b
                    .data
                    .get("ts")
                    .and_then(|v| v.as_str()).map_or_else(|| b.ts.clone(), std::string::ToString::to_string);
                edges_all.push(Edge {
                    source: a.id.clone(),
                    target: b.id.clone(),
                    kind: "next_in_session".to_string(),
                    ts,
                });
            }
        }
    }
}

/// Full-corpus `(by_type, by_outcome)` tallies over every node.
fn corpus_stats(
    nodes: &HashMap<String, Node>,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let mut corpus_by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut corpus_by_outcome: BTreeMap<String, usize> = BTreeMap::new();
    for n in nodes.values() {
        *corpus_by_type.entry(n.kind.clone()).or_insert(0) += 1;
        if let Some(outcome) = n.data.get("outcome").and_then(|v| v.as_str()) {
            *corpus_by_outcome.entry(outcome.to_string()).or_insert(0) += 1;
        }
    }
    (corpus_by_type, corpus_by_outcome)
}

/// Stage 4 — pick the visible window: the `K_SESSIONS` most-recently
/// active sessions, capped per-session (the focused session gets the
/// larger cap and is forced in even when it has fallen out of the
/// top-K). Returns the initial kept-id set plus the ordered top-session
/// list for the later ticker expansion.
fn select_window(
    nodes: &HashMap<String, Node>,
    edges_all: &[Edge],
    opts: &GraphOpts,
) -> (HashSet<String>, Vec<String>) {
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
    sessions_by_recency.sort_by_key(|s| std::cmp::Reverse(s.1));
    let top_sids: Vec<String> = sessions_by_recency
        .into_iter()
        .take(K_SESSIONS)
        .map(|(s, _)| s)
        .collect();
    let mut kept_inv_ids: HashSet<String> = HashSet::new();
    for sid in &top_sids {
        // Focused session gets a larger window than the others so the
        // operator sees deep context for what they're looking at,
        // while peripheral sessions stay readable at 12 nodes each.
        let cap = if Some(sid) == opts.focused_session.as_ref() {
            PER_SESSION_CAP_FOCUSED
        } else {
            PER_SESSION_CAP_DEFAULT
        };
        if let Some(invs) = inv_by_session.get(sid) {
            let mut sorted: Vec<&&Node> = invs.iter().collect();
            sorted.sort_by_key(|n| std::cmp::Reverse(n.seq));
            for n in sorted.into_iter().take(cap) {
                kept_inv_ids.insert(n.id.clone());
            }
        }
    }
    // If a focused session was requested AND isn't in the top-K
    // (possible when it's been quiet recently), include it anyway with
    // the focused cap. This keeps the user's currently-selected session
    // present even if other galaxies are more active.
    if let Some(focus) = opts.focused_session.as_ref() {
        if !top_sids.contains(focus) {
            if let Some(invs) = inv_by_session.get(focus) {
                let mut sorted: Vec<&&Node> = invs.iter().collect();
                sorted.sort_by_key(|n| std::cmp::Reverse(n.seq));
                for n in sorted.into_iter().take(PER_SESSION_CAP_FOCUSED) {
                    kept_inv_ids.insert(n.id.clone());
                }
            }
        }
    }
    let mut kept_session_ids: HashSet<String> = HashSet::new();
    for e in edges_all {
        if kept_inv_ids.contains(&e.target) && e.source.starts_with(node_kind::SESSION) {
            kept_session_ids.insert(e.source.clone());
        }
    }
    let kept_ids: HashSet<String> = kept_inv_ids.union(&kept_session_ids).cloned().collect();
    (kept_ids, top_sids)
}

/// Stage 5 — apply the time floor + hook-hide policy to `recent_events`
/// (mutating it in place for the later ticker tail), then expand
/// `kept_ids` by walking the visible event tail newest-first, honouring
/// the per-session tool-call caps. Sessions outside `top_sids ∪
/// focused` are skipped so we never drag in TCs from a dropped session.
fn expand_window_from_ticker(
    nodes: &HashMap<String, Node>,
    recent_events: &mut Vec<RecentEvent>,
    opts: &GraphOpts,
    limit: usize,
    top_sids: &[String],
    kept_ids: &mut HashSet<String>,
) {
    // Per-event timestamp resolution. `recent_events.ts` is the SQL
    // column, which the bridge leaves empty for `sentinel.*` events;
    // the payload always carries `ts_sec` and/or `ts`.
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
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
    // showing hook nodes, drop hook-level events too. Tool-call events
    // already carry n_hooks + outcomes, so nothing is lost.
    if !opts.include_hooks {
        recent_events.retain(|ev| ev.kind != kind::HOOK_INGESTED && ev.kind != kind::HOOK_DENIED);
    }

    // Compute the visible event tail (matches what the ticker will show).
    let events_limit_for_window = (limit * 6).max(600);
    let events_tail_for_window: &[RecentEvent] = if recent_events.len() > events_limit_for_window {
        &recent_events[recent_events.len() - events_limit_for_window..]
    } else {
        &recent_events[..]
    };

    let visible_session_ids: HashSet<String> = top_sids
        .iter()
        .chain(opts.focused_session.iter())
        .cloned()
        .collect();
    let mut tc_count_per_session: HashMap<String, usize> = HashMap::new();
    // Walk newest-first so we keep the freshest TCs per session.
    for ev in events_tail_for_window.iter().rev() {
        let Some(ev_sid) = ev.payload.get("session_id").and_then(|v| v.as_str()) else { continue };
        if ev_sid.is_empty() || !visible_session_ids.contains(ev_sid) {
            continue;
        }
        // Pull the session node into the window so its label renders.
        for n in nodes.values() {
            if n.kind == node_kind::SESSION
                && n.data.get("session_id").and_then(|v| v.as_str()) == Some(ev_sid)
            {
                kept_ids.insert(n.id.clone());
            }
        }
        // Apply per-session TC cap.
        let cap = if Some(ev_sid) == opts.focused_session.as_deref() {
            PER_SESSION_CAP_FOCUSED
        } else {
            PER_SESSION_CAP_DEFAULT
        };
        let count = tc_count_per_session.entry(ev_sid.to_string()).or_insert(0);
        if *count >= cap {
            continue;
        }
        if let Some(tcid) = ev.payload.get("tool_call_id").and_then(|v| v.as_str()) {
            if nodes.contains_key(tcid) && !kept_ids.contains(tcid) {
                kept_ids.insert(tcid.to_string());
                *count += 1;
            }
        }
        if let Some(hid) = ev.payload.get("invocation_id").and_then(|v| v.as_str()) {
            if nodes.contains_key(hid) {
                kept_ids.insert(hid.to_string());
            }
        }
    }
}

/// Annotate tool-call nodes with a coarse category so the UI can colour
/// by intent without inspecting `tool` client-side.
fn annotate_categories(kept_nodes: &mut [Node]) {
    for n in kept_nodes.iter_mut() {
        if n.kind != node_kind::TOOL_CALL {
            continue;
        }
        let tool = n.data.get("tool").and_then(|v| v.as_str()).unwrap_or("");
        let sev = n.data.get("sentinel_event").and_then(|v| v.as_str());
        n.category = Some(NodeCategory::from_tool(tool, sev));
    }
}

/// Default-hide hooks: drop `SentinelHookInvocation` nodes + their
/// edges, then synthesise direct session → tool-call edges by walking
/// the (session → hook → tool-call) chain from the full edge list. This
/// keeps the canvas legible (the bridge produces ~10× as many hooks as
/// tool-calls).
fn collapse_hooks_into_synth_edges(
    kept_nodes: &mut Vec<Node>,
    kept_edges: &mut Vec<Edge>,
    edges_all: &[Edge],
    kept_ids: &HashSet<String>,
) {
    // Build the mapping while we still have all the data.
    let mut hook_session: HashMap<&str, &str> = HashMap::new();
    let mut hook_to_tc: HashMap<&str, &str> = HashMap::new();
    for e in edges_all {
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

/// Windowed `(by_type, by_outcome)` tallies over the kept nodes only.
fn window_stats(kept_nodes: &[Node]) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_outcome: BTreeMap<String, usize> = BTreeMap::new();
    for n in kept_nodes {
        *by_type.entry(n.kind.clone()).or_insert(0) += 1;
        if let Some(outcome) = n.data.get("outcome").and_then(|v| v.as_str()) {
            *by_outcome.entry(outcome.to_string()).or_insert(0) += 1;
        }
    }
    (by_type, by_outcome)
}

/// Per-session liveness annotation: derive each session node's status
/// (firing/busy/idle/dormant/dead, or awaiting-user) from its most
/// recent hook timestamp and transcript mtime, reading the full node
/// map for the per-sid hook maxima.
fn annotate_liveness(kept_nodes: &mut [Node], nodes: &HashMap<String, Node>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());

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
        let Some(sid) = n.data.get("session_id").and_then(|v| v.as_str()).map(std::string::ToString::to_string)
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
            .map_or(0.0, |d| d.as_secs_f64());
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
}

/// Build the ticker tail. Per user direction 2026-05-25: up to 5
/// sessions × 20 events each = 100 datapoints in the active view. Walk
/// newest-first, keeping at most `PER_SESSION_CAP_DEFAULT` per
/// `session_id`, then flip back to chronological so the ticker's
/// "newest at top" reverse-loop in JS still works.
fn build_ticker_tail(recent_events: Vec<RecentEvent>) -> Vec<RecentEvent> {
    let mut per_sid_count: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<RecentEvent> = Vec::with_capacity(100);
    for ev in recent_events.into_iter().rev() {
        if kept.len() >= 100 {
            break;
        }
        let sid = ev
            .payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let count = per_sid_count.entry(sid.clone()).or_insert(0);
        if *count >= PER_SESSION_CAP_DEFAULT {
            continue;
        }
        *count += 1;
        kept.push(ev);
    }
    kept.reverse();
    kept
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
        .map_or(0.0, |d| d.timestamp() as f64 + f64::from(d.timestamp_subsec_micros()) / 1_000_000.0)
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
