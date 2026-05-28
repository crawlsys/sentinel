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

/// Process-wide lock serialising `$HOME` mutation across the tests in
/// this binary. `cargo test` runs test fns on multiple threads, and
/// `set_var` is process-global, so without this the two fixtures race:
/// one restores HOME while the other is mid-build, leaking the live
/// metric files back in. Holding this for the whole test body makes the
/// HOME swap atomic w.r.t. the sibling test.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Repoint `$HOME` at an empty dir for the life of the returned guard.
///
/// `graph::load_graph_with` calls `augment_from_metric_files`, which reads
/// `$HOME/.claude/sentinel/metrics/*.jsonl` (and five sibling harness homes).
/// On a host that actually has those files — e.g. a live sandbox — they leak
/// tens of thousands of real events into what is supposed to be a hermetic
/// 11-event fixture, blowing the `max_seq` and node-kind assertions. Pinning
/// HOME to an empty TempDir makes the metric-file read a no-op so the fixture
/// reflects only the store it built. Restores the prior HOME on drop.
struct HomeGuard {
    prev: Option<std::ffi::OsString>,
    _tmp: TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HomeGuard {
    fn empty() -> Self {
        // Recover from a poisoned lock — a panicking sibling test must
        // not wedge the rest of the suite.
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var_os("HOME");
        // SAFETY: the HOME_LOCK guard above serialises this mutation
        // against the only other code in this binary that touches HOME.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        Self { prev, _tmp: tmp, _lock: lock }
    }
}

impl HomeGuard {
    /// Repoint `$HOME` at a temp dir that *contains* a synthetic
    /// `~/.claude/sentinel/metrics/{sessions.jsonl,hook-invocations.jsonl}`,
    /// so `augment_from_metric_files` has live data to fold in. Used to
    /// exercise the metric-file fallback path — the one that surfaces
    /// JSONL sessions into the per-session feed when the SQLite store is
    /// thin or empty (the real shape inside a fresh sandbox).
    fn with_metrics(sessions_jsonl: &str, hooks_jsonl: &str) -> Self {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let metrics = tmp.path().join(".claude/sentinel/metrics");
        std::fs::create_dir_all(&metrics).unwrap();
        std::fs::write(metrics.join("sessions.jsonl"), sessions_jsonl).unwrap();
        std::fs::write(metrics.join("hook-invocations.jsonl"), hooks_jsonl).unwrap();
        let prev = std::env::var_os("HOME");
        // SAFETY: serialised by HOME_LOCK, as in `empty()`.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        Self { prev, _tmp: tmp, _lock: lock }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

#[test]
fn graph_default_hides_hooks_and_synthesises_session_to_tc_edges() {
    let _home = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fixture.db");
    build_store(&path);

    let conn = db::open_ro(&path).unwrap();
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit: 50, since_secs: None, include_hooks: false, focused_session: None },
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
    let _home = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fixture.db");
    build_store(&path);

    let conn = db::open_ro(&path).unwrap();
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit: 50, since_secs: None, include_hooks: true, focused_session: None },
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
        GraphOpts { limit: 50, since_secs: Some(1), include_hooks: false, focused_session: None },
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

/// Create an empty-but-openable store: bridge schema, zero rows.
fn build_empty_store(path: &std::path::Path) {
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
        );",
    )
    .unwrap();
}

/// The per-session feed (the highest-signal status model) must still
/// light up when the SQLite store carries no rows yet but the live
/// JSONL metric files do — the real shape inside a fresh sandbox.
/// `augment_from_metric_files` is the fallback that surfaces those
/// sessions, and it only runs *inside* `load_graph_with`. This pins
/// that a session present only in `hook-invocations.jsonl` reaches the
/// response as a ticker event AND gets a computed `session_status`,
/// so a regression that drops the augmentation can't silently blank
/// the operator's live feed.
#[test]
fn graph_surfaces_metric_file_sessions_with_status() {
    // Fresh timestamps so the default 6h time floor keeps them.
    let now = chrono::Utc::now();
    let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let sid = "metricfile-sess-1";

    let sessions = format!(
        "{}\n",
        serde_json::json!({
            "event": "session_start",
            "session_id": sid,
            "ts": ts,
        })
    );
    let hooks = format!(
        "{}\n",
        serde_json::json!({
            "event": "PreToolUse",
            "hook": "phase_gate",
            "outcome": "allow",
            "session_id": sid,
            "trace_id": "trace-mf-1",
            "duration_us": 1200,
            "ts": ts,
            "tool": "Bash",
        })
    );

    let _home = HomeGuard::with_metrics(&sessions, &hooks);
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("empty.db");
    build_empty_store(&path);

    let conn = db::open_ro(&path).unwrap();
    let g = graph::load_graph_with(
        &conn,
        GraphOpts {
            limit: 50,
            since_secs: Some(6 * 3600),
            include_hooks: true,
            focused_session: None,
        },
    )
    .unwrap();

    assert!(g.error.is_none(), "unexpected error: {:?}", g.error);

    // The hook invocation from the JSONL surfaced as a ticker event.
    assert!(
        g.events.iter().any(|e| e
            .payload
            .get("session_id")
            .and_then(|v| v.as_str())
            == Some(sid)),
        "metric-file session never reached the event tail: {:?}",
        g.events
    );

    // And a synthesised session node carries a computed liveness
    // status — the highest-signal field the operator's feed renders.
    let node = g
        .nodes
        .iter()
        .find(|n| {
            n.kind == "SentinelSession"
                && n.data.get("session_id").and_then(|v| v.as_str()) == Some(sid)
        })
        .expect("expected a synthesised session node for the metric-file session");
    assert!(
        node.session_status.is_some(),
        "metric-file session missing session_status"
    );
}
