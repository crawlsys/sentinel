//! Builds a synthetic 3-session SQLite store on disk, runs
//! `graph::load_graph`, and snapshots the API response shape.

use rusqlite::Connection;
use sentinel_viz_api::graph::GraphOpts;
use sentinel_viz_api::{db, graph};
use tempfile::TempDir;

fn build_store(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            id TEXT NOT NULL,
            type TEXT NOT NULL,
            actor TEXT,
            payload TEXT NOT NULL,
            frame_id TEXT,
            caused_by TEXT,
            timestamp TEXT NOT NULL,
            run_id TEXT NOT NULL,
            UNIQUE(id, run_id)
        );
        CREATE INDEX idx_events_run ON events(run_id, seq);
        CREATE INDEX idx_events_type ON events(type);",
    )
    .unwrap();

    // Two sessions, each with two hook invocations, one with a tool_call.
    let mut idx = 0i64;
    let mut emit = |kind: &str, id: &str, payload: serde_json::Value, ts: &str| {
        idx += 1;
        conn.execute(
            "INSERT INTO events (seq, id, type, payload, timestamp, run_id) \
             VALUES (?, ?, ?, ?, ?, 'fixture-run')",
            rusqlite::params![idx, id, kind, payload.to_string(), ts],
        )
        .unwrap();
    };

    let mk_session = |sid: &str, started: &str| {
        serde_json::json!({
            "object": {
                "id": format!("SentinelSession#{sid}"),
                "type": "SentinelSession",
                "data": {
                    "session_id": sid,
                    "cwd": "/tmp/fixture",
                    "platform": "linux",
                    "started_at": started,
                }
            }
        })
    };
    let mk_hook = |hid: &str, sid: &str, ts: &str, outcome: &str| {
        serde_json::json!({
            "object": {
                "id": format!("SentinelHookInvocation#{hid}"),
                "type": "SentinelHookInvocation",
                "data": {
                    "session_id": sid,
                    "ts": ts,
                    "outcome": outcome,
                    "hook_event": "PreToolUse",
                }
            }
        })
    };
    let mk_tc = |tcid: &str, sid: &str, ts: &str| {
        serde_json::json!({
            "object": {
                "id": format!("SentinelToolCall#{tcid}"),
                "type": "SentinelToolCall",
                "data": {
                    "session_id": sid,
                    "ts": ts,
                    "tool": "Bash",
                }
            }
        })
    };
    let mk_rel = |src: &str, tgt: &str, rtype: &str| {
        serde_json::json!({
            "relation": { "source": src, "target": tgt, "type": rtype }
        })
    };

    emit("object.created", "e1", mk_session("sess-a", "2026-05-25T00:00:00Z"), "2026-05-25T00:00:00Z");
    emit("object.created", "e2", mk_hook("h1", "sess-a", "2026-05-25T00:00:01Z", "allowed"), "2026-05-25T00:00:01Z");
    emit("relation.created", "r1", mk_rel("SentinelSession#sess-a", "SentinelHookInvocation#h1", "has_invocation"), "2026-05-25T00:00:01Z");
    emit("object.created", "e3", mk_hook("h2", "sess-a", "2026-05-25T00:00:02Z", "denied"), "2026-05-25T00:00:02Z");
    emit("relation.created", "r2", mk_rel("SentinelSession#sess-a", "SentinelHookInvocation#h2", "has_invocation"), "2026-05-25T00:00:02Z");
    emit("object.created", "e4", mk_tc("tc1", "sess-a", "2026-05-25T00:00:02Z"), "2026-05-25T00:00:02Z");
    emit("relation.created", "r3", mk_rel("SentinelHookInvocation#h2", "SentinelToolCall#tc1", "has_tool_call"), "2026-05-25T00:00:02Z");
    emit("sentinel.tool_call_observed", "te1", serde_json::json!({
        "tool_call_id": "SentinelToolCall#tc1",
        "session_id": "sess-a",
        "tool": "Bash"
    }), "2026-05-25T00:00:02Z");

    emit("object.created", "e5", mk_session("sess-b", "2026-05-25T00:01:00Z"), "2026-05-25T00:01:00Z");
    emit("object.created", "e6", mk_hook("h3", "sess-b", "2026-05-25T00:01:01Z", "allowed"), "2026-05-25T00:01:01Z");
    emit("relation.created", "r4", mk_rel("SentinelSession#sess-b", "SentinelHookInvocation#h3", "has_invocation"), "2026-05-25T00:01:01Z");
}

#[test]
fn graph_default_hides_hooks_and_synthesises_session_to_tc_edges() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fixture.db");
    build_store(&path);

    let conn = db::open_ro(&path).unwrap();
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit: 50, since_secs: None, include_hooks: false },
    )
    .unwrap();

    assert!(g.error.is_none(), "unexpected error: {:?}", g.error);
    assert_eq!(g.max_seq, 11, "max_seq mismatch");

    let kinds: std::collections::BTreeSet<String> =
        g.nodes.iter().map(|n| n.kind.clone()).collect();
    assert!(kinds.contains("SentinelSession"));
    assert!(kinds.contains("SentinelToolCall"));
    assert!(
        !kinds.contains("SentinelHookInvocation"),
        "hooks should be hidden by default"
    );

    // Synthetic session→tc edge replaces the dropped session→hook→tc chain.
    assert!(
        g.edges.iter().any(|e| e.kind == "has_tool_call_synth"
            && e.source == "SentinelSession#sess-a"
            && e.target == "SentinelToolCall#tc1"),
        "expected synthesised session→tc edge"
    );

    // TC gets categorised (Bash = compute = tc category)
    let tc = g.nodes.iter().find(|n| n.id == "SentinelToolCall#tc1").unwrap();
    assert!(tc.category.is_some());

    for n in &g.nodes {
        if n.kind == "SentinelSession" {
            assert!(n.session_status.is_some(), "session missing status: {}", n.id);
        }
    }
}

#[test]
fn graph_include_hooks_keeps_them_and_derived_chain_edges() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fixture.db");
    build_store(&path);

    let conn = db::open_ro(&path).unwrap();
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit: 50, since_secs: None, include_hooks: true },
    )
    .unwrap();

    let kinds: std::collections::BTreeSet<String> =
        g.nodes.iter().map(|n| n.kind.clone()).collect();
    assert!(kinds.contains("SentinelHookInvocation"));
    assert!(
        g.edges.iter().any(|e| e.kind == "next_in_session"),
        "expected derived next_in_session edge"
    );
}

#[test]
fn graph_time_floor_drops_old_events() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fixture.db");
    build_store(&path);

    let conn = db::open_ro(&path).unwrap();
    // Fixture events are dated 2026-05-25; an aggressive 1-second
    // floor against `now` should drop them all from the ticker.
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit: 50, since_secs: Some(1), include_hooks: false },
    )
    .unwrap();
    assert_eq!(g.events.len(), 0, "time floor should drop ancient events");
}

#[test]
fn graph_empty_db_returns_error_field() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("nope.db");
    let g = graph::load_graph_from_path(&missing, 50).unwrap();
    assert!(g.error.is_some());
    assert!(g.nodes.is_empty());
}
