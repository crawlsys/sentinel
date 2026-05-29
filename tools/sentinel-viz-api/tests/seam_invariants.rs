//! Operator-authored acceptance gate for the "yeet activegraph" refactor.
//!
//! These tests pin the invariants the relational read-model MUST hold.
//! They are written against the STABLE public surface
//! (`graph::load_graph_with` → `GraphResponse`) and a relational
//! fixture built with raw SQL, so they do not depend on bridge
//! internals. They encode DESIRED behaviour: do not weaken an
//! assertion to make failing code pass — fix the code.
//!
//! Background: the old event-sourced store made sessions invisible
//! (0 of 6,271 on the live DB, because session rows fell below the
//! newest-seq read window) and per-session reads cost ~11s
//! (unindexed json_extract scan). The relational schema
//! (sessions + hook_events indexed on (session_id, ts)) exists to
//! make every one of these pass cheaply.

use rusqlite::Connection;
use sentinel_viz_api::graph::GraphOpts;
use sentinel_viz_api::{db, graph};
use tempfile::TempDir;

/// Process-wide lock serialising `$HOME` mutation (see graph_fixture.rs).
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Pin `$HOME` to an empty dir so no live metric files leak into a
/// hermetic fixture. After the refactor the API should not read metric
/// files at all; this is belt-and-suspenders.
struct HomeGuard {
    prev: Option<std::ffi::OsString>,
    _tmp: TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl HomeGuard {
    fn empty() -> Self {
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let prev = std::env::var_os("HOME");
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

/// The relational schema the bridge writes (schema_version=2).
/// Mirrors plans/sentinel-viz-yeet-activegraph.md. Kept here verbatim
/// so the gate is self-contained.
const DDL: &str = "
CREATE TABLE sessions (
  session_id     TEXT PRIMARY KEY,
  source_harness TEXT NOT NULL DEFAULT 'claude',
  cwd            TEXT NOT NULL DEFAULT '',
  platform       TEXT NOT NULL DEFAULT '',
  started_at     TEXT NOT NULL DEFAULT '',
  last_activity_ts TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_sessions_activity ON sessions(last_activity_ts);
CREATE TABLE hook_events (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id     TEXT NOT NULL,
  ts             TEXT NOT NULL,
  sentinel_event TEXT NOT NULL DEFAULT '',
  hook           TEXT NOT NULL DEFAULT '',
  tool           TEXT NOT NULL DEFAULT '',
  outcome        TEXT NOT NULL DEFAULT 'allow',
  duration_us    INTEGER NOT NULL DEFAULT 0,
  trace_id       TEXT NOT NULL,
  source_harness TEXT NOT NULL DEFAULT 'claude',
  UNIQUE(trace_id)
);
CREATE INDEX idx_hook_session_ts ON hook_events(session_id, ts);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

fn iso(secs_ago: i64) -> String {
    let t = chrono::Utc::now() - chrono::Duration::seconds(secs_ago);
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

struct Fixture {
    conn: Connection,
    trace: u64,
}
impl Fixture {
    fn new(path: &std::path::Path) -> Self {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(DDL).unwrap();
        conn.execute(
            "INSERT INTO meta(key,value) VALUES('schema_version','2')",
            [],
        )
        .unwrap();
        Self { conn, trace: 0 }
    }
    fn session(&self, sid: &str, harness: &str, cwd: &str, started_ago: i64, last_ago: i64) {
        self.conn
            .execute(
                "INSERT INTO sessions(session_id,source_harness,cwd,platform,started_at,last_activity_ts)
                 VALUES (?,?,?,?,?,?)",
                rusqlite::params![sid, harness, cwd, "linux", iso(started_ago), iso(last_ago)],
            )
            .unwrap();
    }
    fn hooks(&mut self, sid: &str, harness: &str, n: usize, newest_ago: i64) {
        let tx = &self.conn;
        for i in 0..n {
            self.trace += 1;
            // spread events backwards from newest_ago
            let ts = iso(newest_ago + i as i64);
            tx.execute(
                "INSERT INTO hook_events(session_id,ts,sentinel_event,hook,tool,outcome,duration_us,trace_id,source_harness)
                 VALUES (?,?,?,?,?,?,?,?,?)",
                rusqlite::params![sid, ts, "PreToolUse", "phase_gate", "Bash", "allow", 1200i64, format!("tr-{}", self.trace), harness],
            )
            .unwrap();
        }
    }
}

fn load(path: &std::path::Path, limit: usize) -> sentinel_viz_api::model::GraphResponse {
    let conn = db::open_ro(path).unwrap();
    graph::load_graph_with(
        &conn,
        GraphOpts { limit, since_secs: Some(6 * 3600), focused_session: None, ..GraphOpts::default() },
    )
    .unwrap()
}

fn session_nodes(g: &sentinel_viz_api::model::GraphResponse) -> Vec<&sentinel_viz_api::model::Node> {
    g.nodes.iter().filter(|n| n.kind == "SentinelSession").collect()
}
fn sid_of(n: &sentinel_viz_api::model::Node) -> Option<&str> {
    n.data.get("session_id").and_then(|v| v.as_str())
}

/// INVARIANT 1: more than the old K_SESSIONS=5 cap are all visible.
#[test]
fn all_active_sessions_visible_no_topk_truncation() {
    let _h = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fx.db");
    let mut fx = Fixture::new(&path);
    for i in 0..8 {
        let sid = format!("sess-{i}");
        fx.session(&sid, "claude", "/tmp/work", 600, 10 + i as i64);
        fx.hooks(&sid, "claude", 3, 10 + i as i64);
    }
    drop(fx);
    let g = load(&path, 6000);
    let sids: std::collections::BTreeSet<String> =
        session_nodes(&g).iter().filter_map(|n| sid_of(n).map(str::to_string)).collect();
    assert_eq!(sids.len(), 8, "expected all 8 active sessions, got {}: {sids:?}", sids.len());
}

/// INVARIANT 2: a session recent by last_activity_ts is visible even
/// if it was inserted first (low rowid). The old seq-window dropped it.
#[test]
fn session_visible_by_activity_not_insertion_order() {
    let _h = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fx.db");
    let mut fx = Fixture::new(&path);
    // Inserted first, but most-recently active.
    fx.session("old-but-active", "claude", "/tmp/a", 9000, 5);
    fx.hooks("old-but-active", "claude", 2, 5);
    // Lots of newer rows for other sessions.
    for i in 0..20 {
        let sid = format!("noise-{i}");
        fx.session(&sid, "claude", "/tmp/n", 600, 30 + i as i64);
        fx.hooks(&sid, "claude", 5, 30 + i as i64);
    }
    drop(fx);
    let g = load(&path, 6000);
    assert!(
        session_nodes(&g).iter().any(|n| sid_of(n) == Some("old-but-active")),
        "session recent by last_activity_ts was dropped"
    );
}

/// INVARIANT 3 + 4: codex session present, tagged, with metadata.
#[test]
fn codex_session_present_and_tagged() {
    let _h = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fx.db");
    let mut fx = Fixture::new(&path);
    fx.session("019d-codex", "codex", "/home/u/proj", 300, 8);
    fx.hooks("019d-codex", "codex", 4, 8);
    drop(fx);
    let g = load(&path, 6000);
    let node = session_nodes(&g)
        .into_iter()
        .find(|n| sid_of(n) == Some("019d-codex"))
        .expect("codex session missing");
    assert_eq!(
        node.data.get("source_harness").and_then(|v| v.as_str()),
        Some("codex"),
        "codex session not tagged source_harness=codex"
    );
    assert_eq!(
        node.data.get("cwd").and_then(|v| v.as_str()),
        Some("/home/u/proj"),
        "codex session lost cwd metadata"
    );
}

/// INVARIANT 5: every session node carries a liveness status.
#[test]
fn every_session_has_liveness_status() {
    let _h = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fx.db");
    let mut fx = Fixture::new(&path);
    fx.session("live-1", "claude", "/tmp", 120, 2);
    fx.hooks("live-1", "claude", 3, 2);
    drop(fx);
    let g = load(&path, 6000);
    for n in session_nodes(&g) {
        assert!(n.session_status.is_some(), "session {:?} missing status", sid_of(n));
    }
}

/// INVARIANT 6: no hidden per-session event cap. 300 events for one
/// session must all surface when the request limit is generous.
#[test]
fn no_hidden_per_session_event_cap() {
    let _h = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fx.db");
    let mut fx = Fixture::new(&path);
    fx.session("deep", "claude", "/tmp", 600, 1);
    fx.hooks("deep", "claude", 300, 1);
    drop(fx);
    let g = load(&path, 6000);
    let n = g
        .events
        .iter()
        .filter(|e| e.payload.get("session_id").and_then(|v| v.as_str()) == Some("deep"))
        .count();
    assert!(n >= 300, "expected >=300 events for the session (no 150 cap), got {n}");
}

/// INVARIANT 7: perf. ~100k events across 50 sessions, bounded read
/// must be fast (indexed). Budget is generous to avoid CI flake but
/// far below the old full-scan behaviour.
#[test]
fn load_graph_is_fast_on_large_store() {
    let _h = HomeGuard::empty();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("fx.db");
    let mut fx = Fixture::new(&path);
    fx.conn.execute_batch("BEGIN").unwrap();
    for i in 0..50 {
        let sid = format!("s-{i}");
        fx.session(&sid, "claude", "/tmp", 3000, 5 + i as i64);
        fx.hooks(&sid, "claude", 2000, 5 + i as i64);
    }
    fx.conn.execute_batch("COMMIT").unwrap();
    drop(fx);
    let start = std::time::Instant::now();
    let g = load(&path, 6000);
    let elapsed = start.elapsed();
    assert!(!session_nodes(&g).is_empty(), "no sessions returned");
    assert!(
        elapsed.as_millis() < 500,
        "load_graph took {}ms on 100k events — likely an unindexed scan",
        elapsed.as_millis()
    );
}
