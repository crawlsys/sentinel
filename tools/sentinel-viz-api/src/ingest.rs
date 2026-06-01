//! Sentinel metrics → `events` store ingester.
//!
//! Reads the sentinel hook-invocation and session JSONL metrics and writes
//! the same `events` `SQLite` schema this crate's API already reads (see
//! `db::read_events` / `graph::load_graph`). This is the Rust replacement for
//! the former `tools/sentinel-viz/sentinel_bridge.py` `ActiveGraph` bridge —
//! it writes the `events` table directly rather than going through the Python
//! `activegraph` abstraction.
//!
//! Behaviour is a 1:1 port of the bridge:
//! - reads both the real (`.claude`) and sandbox (`.claude-sentinel`)
//!   metrics dirs, merged by file mtime ascending so newest data sorts last;
//! - dedupes hook invocations on the composite key `(trace_id, hook, event,
//!   ts)` — `trace_id` alone is a shared correlation id, not unique per fire;
//! - dedupes sessions by `session_id`, creating a stub `SentinelSession` for
//!   any hook whose `session_id` was never seen in `sessions.jsonl`;
//! - emits three object/relation/domain row shapes the reader parses;
//! - supports a one-shot mode (read all, ingest, print summary) and a
//!   `--tail` mode (seed from existing, then poll every 1s with per-file byte
//!   offsets, resetting to 0 on rotation/truncation).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// `run_id` written to every row. The reader reads all events ordered by
/// `seq` regardless of `run_id`, so a single stable value keeps resumable
/// imports collision-free under the `UNIQUE(id, run_id)` constraint (event
/// ids are seeded from the existing row count, which only grows).
const RUN_ID: &str = "sentinel-bridge";

/// Object `type` discriminators (mirrors `model::node_kind`).
const NODE_SESSION: &str = "SentinelSession";
const NODE_HOOK: &str = "SentinelHookInvocation";

/// Poll interval for `--tail`.
const TAIL_POLL: Duration = Duration::from_secs(1);

/// Composite dedupe key for a hook invocation: `(trace_id, hook, event, ts)`.
type HookKey = (String, String, String, String);

/// Resolved metrics file locations across both Claude homes.
pub struct MetricsPaths {
    /// `hook-invocations.jsonl` in each metrics dir that this layout knows.
    pub hook_files: Vec<PathBuf>,
    /// `sessions.jsonl` in each metrics dir.
    pub session_files: Vec<PathBuf>,
}

impl MetricsPaths {
    /// Resolve the default metrics locations from the home directory.
    ///
    /// Honors the `SENTINEL_VIZ_HOME` override first (used by tests and any
    /// non-standard layout), then falls back to the OS home — the same
    /// pattern `transcript::find_transcript` uses, which matters on Windows
    /// where `dirs::home_dir()` ignores `$HOME`.
    pub fn from_home() -> Result<Self> {
        let home = viz_home().context("could not determine home directory")?;
        let dirs = [
            home.join(".claude/sentinel/metrics"),
            home.join(".claude-sentinel/sentinel/metrics"),
        ];
        Ok(Self {
            hook_files: dirs.iter().map(|d| d.join("hook-invocations.jsonl")).collect(),
            session_files: dirs.iter().map(|d| d.join("sessions.jsonl")).collect(),
        })
    }
}

/// Resolve the home directory, honoring `SENTINEL_VIZ_HOME` first.
fn viz_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("SENTINEL_VIZ_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir()
}

// ── JSONL reading ────────────────────────────────────────────────────────

/// Parse one JSONL file, skipping blank lines and any row that fails to
/// decode (resilient to a half-written final line on a tailed read).
fn read_jsonl(path: &Path) -> Vec<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_lines(text.lines())
}

/// Parse an iterator of raw lines into JSON values, skipping blanks/bad rows.
fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<serde_json::Value> {
    lines
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                None
            } else {
                serde_json::from_str(l).ok()
            }
        })
        .collect()
}

/// Read multiple JSONL files merged by mtime ascending (newest sorts last).
fn read_merged(paths: &[PathBuf]) -> Vec<serde_json::Value> {
    let mut with_mtime: Vec<(&PathBuf, std::time::SystemTime)> = paths
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()).map(|t| (p, t)))
        .collect();
    with_mtime.sort_by_key(|&(_, t)| t);
    let mut out = Vec::new();
    for (p, _) in with_mtime {
        out.extend(read_jsonl(p));
    }
    out
}

// ── record field access (mirrors Python `.get(key, default)`) ────────────

fn str_field(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("").to_string()
}

fn num_field(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(serde_json::Value::as_i64).unwrap_or(0)
}

/// Composite dedupe key for a hook record.
fn hook_key(h: &serde_json::Value) -> HookKey {
    (
        str_field(h, "trace_id"),
        str_field(h, "hook"),
        str_field(h, "event"),
        str_field(h, "ts"),
    )
}

// ── the writer ───────────────────────────────────────────────────────────

/// Stateful ingester bound to one open store. Holds the dedupe sets and the
/// `session_id → object_id` map so one-shot and tail share insert logic.
struct Ingestor<'c> {
    conn: &'c Connection,
    /// Next event-`id` counter; seeded from the existing row count so it never
    /// collides under `UNIQUE(id, run_id)` across resumed imports.
    next_id: u64,
    /// Already-ingested hook keys (seeded from existing hook object rows).
    seen_hooks: HashSet<HookKey>,
    /// `session_id → SentinelSession object id`.
    session_map: HashMap<String, String>,
}

impl<'c> Ingestor<'c> {
    /// Open the store (creating schema if absent) and seed dedupe state from
    /// any rows already present, making imports resumable.
    fn attach(conn: &'c Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
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
            CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, seq);
            CREATE INDEX IF NOT EXISTS idx_events_type ON events(type);",
        )
        .context("creating events schema")?;

        let next_id: u64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
            .max(0)
            .unsigned_abs();

        let mut me = Self {
            conn,
            next_id,
            seen_hooks: HashSet::new(),
            session_map: HashMap::new(),
        };
        me.seed_from_store()?;
        Ok(me)
    }

    /// Seed `seen_hooks` and `session_map` from existing `object.created` rows.
    fn seed_from_store(&mut self) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload FROM events WHERE type = 'object.created'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            let Ok(text) = row else { continue };
            let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(obj) = payload.get("object") else { continue };
            let otype = obj.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
            let data = obj.get("data").cloned().unwrap_or(serde_json::Value::Null);
            match otype {
                NODE_SESSION => {
                    let sid = str_field(&data, "session_id");
                    if let Some(id) = obj.get("id").and_then(serde_json::Value::as_str) {
                        if !sid.is_empty() {
                            self.session_map.entry(sid).or_insert_with(|| id.to_string());
                        }
                    }
                }
                NODE_HOOK => {
                    self.seen_hooks.insert(hook_key(&data));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Allocate the next unique event id.
    fn event_id(&mut self) -> String {
        let id = format!("ev{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Insert one row. `actor`/`timestamp` are `None`/empty for `sentinel.*`
    /// domain events per the reader contract (the payload carries the ts).
    fn emit(
        &mut self,
        kind: &str,
        payload: &serde_json::Value,
        actor: Option<&str>,
        timestamp: &str,
    ) -> Result<()> {
        let id = self.event_id();
        self.conn
            .execute(
                "INSERT INTO events (id, type, actor, payload, frame_id, caused_by, timestamp, run_id) \
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6)",
                rusqlite::params![id, kind, actor, payload.to_string(), timestamp, RUN_ID],
            )
            .with_context(|| format!("inserting {kind} event"))?;
        Ok(())
    }

    /// Create a `SentinelSession` object (+ `sentinel.session_started`) for
    /// each new session record. Returns the number of new sessions created.
    fn ingest_sessions(&mut self, sessions: &[serde_json::Value]) -> Result<usize> {
        let mut added = 0;
        for s in sessions {
            let sid = {
                let raw = str_field(s, "session_id");
                if raw.is_empty() { "unknown".to_string() } else { raw }
            };
            if self.session_map.contains_key(&sid) {
                continue;
            }
            let object_id = format!("{NODE_SESSION}#{sid}");
            let started_at = str_field(s, "ts");
            let payload = serde_json::json!({
                "object": {
                    "id": object_id,
                    "type": NODE_SESSION,
                    "data": {
                        "session_id": sid,
                        "cwd": str_field(s, "cwd"),
                        "platform": str_field(s, "platform"),
                        "started_at": started_at,
                    }
                }
            });
            self.emit("object.created", &payload, Some("sentinel_bridge"), &started_at)?;
            self.session_map.insert(sid.clone(), object_id);

            let domain = serde_json::json!({ "session_id": sid, "ts": started_at });
            self.emit("sentinel.session_started", &domain, None, "")?;
            added += 1;
        }
        Ok(added)
    }

    /// Ensure a `SentinelSession` exists for `sid`, creating a stub if the
    /// session was never seen in `sessions.jsonl`. Returns its object id.
    fn ensure_session(&mut self, sid: &str) -> Result<String> {
        if let Some(id) = self.session_map.get(sid) {
            return Ok(id.clone());
        }
        let object_id = format!("{NODE_SESSION}#{sid}");
        let payload = serde_json::json!({
            "object": {
                "id": object_id,
                "type": NODE_SESSION,
                "data": {
                    "session_id": sid,
                    "cwd": "",
                    "platform": "",
                    "started_at": "",
                }
            }
        });
        self.emit("object.created", &payload, Some("sentinel_bridge"), "")?;
        self.session_map.insert(sid.to_string(), object_id.clone());
        Ok(object_id)
    }

    /// Create a `SentinelHookInvocation` object, link it to its session, and
    /// emit the `sentinel.hook_ingested` (+ `sentinel.hook_denied`) domain
    /// events. Returns the number of new invocations ingested.
    fn ingest_hooks(&mut self, hooks: &[serde_json::Value]) -> Result<usize> {
        let mut added = 0;
        for h in hooks {
            let key = hook_key(h);
            if self.seen_hooks.contains(&key) {
                continue;
            }
            self.seen_hooks.insert(key);

            let sid = {
                let raw = str_field(h, "session_id");
                if raw.is_empty() { "unknown".to_string() } else { raw }
            };
            let session_obj = self.ensure_session(&sid)?;

            let hook = str_field(h, "hook");
            let event = str_field(h, "event");
            let outcome = str_field(h, "outcome");
            let trace_id = str_field(h, "trace_id");
            let duration_us = num_field(h, "duration_us");
            let ts = str_field(h, "ts");

            let hook_obj_id = format!("{NODE_HOOK}#ev{}", self.next_id);
            let payload = serde_json::json!({
                "object": {
                    "id": hook_obj_id,
                    "type": NODE_HOOK,
                    "data": {
                        "hook": hook,
                        "event": event,
                        "outcome": outcome,
                        "session_id": sid,
                        "trace_id": trace_id,
                        "duration_us": duration_us,
                        "repo_root": str_field(h, "repo_root"),
                        "ts": ts,
                    }
                }
            });
            self.emit("object.created", &payload, Some("sentinel_bridge"), &ts)?;

            let relation = serde_json::json!({
                "relation": {
                    "source": session_obj,
                    "target": hook_obj_id,
                    "type": "has_invocation",
                }
            });
            self.emit("relation.created", &relation, Some("sentinel_bridge"), &ts)?;

            let ingested = serde_json::json!({
                "hook": hook,
                "sentinel_event": event,
                "outcome": outcome,
                "session_id": sid,
                "trace_id": trace_id,
                "duration_us": duration_us,
                "ts": ts,
            });
            self.emit("sentinel.hook_ingested", &ingested, None, "")?;

            if outcome == "deny" {
                let denied = serde_json::json!({
                    "hook": hook,
                    "sentinel_event": event,
                    "session_id": sid,
                    "trace_id": trace_id,
                    "ts": ts,
                });
                self.emit("sentinel.hook_denied", &denied, None, "")?;
            }
            added += 1;
        }
        Ok(added)
    }

    /// Count materialised objects of a given type (for the summary).
    fn count_objects(&self, otype: &str) -> i64 {
        let pattern = format!("%\"type\":\"{otype}\"%");
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'object.created' AND payload LIKE ?1",
                rusqlite::params![pattern],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }
}

// ── store path ─────────────────────────────────────────────────────────

/// Open the store read-write, creating the parent directory if needed.
fn open_rw(store: &Path) -> Result<Connection> {
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating store dir {}", parent.display()))?;
    }
    Connection::open(store).with_context(|| format!("opening store {}", store.display()))
}

// ── modes ────────────────────────────────────────────────────────────────

/// One-shot import: read all JSONL, ingest, print a summary.
pub fn run_one_shot(store: &Path, paths: &MetricsPaths) -> Result<()> {
    let conn = open_rw(store)?;
    let mut ing = Ingestor::attach(&conn)?;

    let sessions = read_merged(&paths.session_files);
    let hooks = read_merged(&paths.hook_files);

    ing.ingest_sessions(&sessions)?;
    let new_hooks = ing.ingest_hooks(&hooks)?;

    let n_sessions = ing.count_objects(NODE_SESSION);
    let n_hooks = ing.count_objects(NODE_HOOK);

    println!(
        "[sentinel-bridge] Ingested {n_sessions} sessions, {n_hooks} hook invocations ({new_hooks} new)"
    );
    println!("[sentinel-bridge] Store: {}", store.display());
    print_summary(&conn);
    Ok(())
}

/// Live-tail: seed from existing data, then poll every 1s, applying byte
/// offsets per file and resetting on rotation. Runs until the process is
/// interrupted (Ctrl+C).
pub fn run_tail(store: &Path, paths: &MetricsPaths) -> Result<()> {
    println!("[sentinel-bridge] Tail mode — watching:");
    for p in &paths.hook_files {
        let state = if p.exists() { "(exists)" } else { "(absent — will pick up when created)" };
        println!("  {} {state}", p.display());
    }
    println!("[sentinel-bridge] Store: {}", store.display());
    println!("Press Ctrl+C to stop.\n");

    let conn = open_rw(store)?;
    let mut ing = Ingestor::attach(&conn)?;

    // Seed from existing data first.
    ing.ingest_sessions(&read_merged(&paths.session_files))?;
    ing.ingest_hooks(&read_merged(&paths.hook_files))?;

    // Per-file byte offsets, primed at current EOF so we only push new rows.
    let mut hook_offsets: HashMap<&Path, u64> =
        paths.hook_files.iter().map(|p| (p.as_path(), file_size(p))).collect();
    let mut sess_offsets: HashMap<&Path, u64> =
        paths.session_files.iter().map(|p| (p.as_path(), file_size(p))).collect();

    loop {
        std::thread::sleep(TAIL_POLL);

        for p in &paths.session_files {
            let offset = sess_offsets.entry(p.as_path()).or_insert(0);
            if let Some((lines, new_size)) = read_new(p, *offset) {
                *offset = new_size;
                ing.ingest_sessions(&parse_lines(lines.lines()))?;
            }
        }

        for p in &paths.hook_files {
            let offset = hook_offsets.entry(p.as_path()).or_insert(0);
            if let Some((lines, new_size)) = read_new(p, *offset) {
                *offset = new_size;
                let added = ing.ingest_hooks(&parse_lines(lines.lines()))?;
                if added > 0 {
                    let now = chrono::Local::now().format("%H:%M:%S");
                    let tag = if p.to_string_lossy().contains(".claude-sentinel") {
                        "sandbox"
                    } else {
                        "real"
                    };
                    println!("[{now}] +{added} hook invocations ({tag})");
                }
            }
        }
    }
}

/// Current size of a file in bytes (0 if missing/unreadable).
fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Read new bytes since `offset`, returning `(text, new_size)`. On a size
/// below `offset` (rotation/truncation) the read restarts from byte 0.
/// Returns `None` when there is nothing new.
fn read_new(path: &Path, offset: u64) -> Option<(String, u64)> {
    let new_size = std::fs::metadata(path).ok()?.len();
    let cur = if new_size < offset { 0 } else { offset };
    if new_size <= cur {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(cur)).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    Some((buf, new_size))
}

// ── summary printout ─────────────────────────────────────────────────────

/// Print by-event / by-hook / by-outcome breakdowns, descending by count.
fn print_summary(conn: &Connection) {
    let invocations = hook_data_rows(conn);

    let mut by_event: BTreeMap<String, i64> = BTreeMap::new();
    let mut by_hook: BTreeMap<String, i64> = BTreeMap::new();
    let mut by_outcome: BTreeMap<String, i64> = BTreeMap::new();
    for data in &invocations {
        *by_event.entry(field_or(data, "event")).or_default() += 1;
        *by_hook.entry(field_or(data, "hook")).or_default() += 1;
        *by_outcome.entry(field_or(data, "outcome")).or_default() += 1;
    }

    println!("\n─── Hook invocations by lifecycle event ─────────────────────────");
    for (name, count) in sorted_desc(&by_event) {
        println!("  {name:<30}  {count:>4}");
    }
    println!("\n─── Hook invocations by hook name ───────────────────────────────");
    for (name, count) in sorted_desc(&by_hook).into_iter().take(20) {
        println!("  {name:<40}  {count:>4}");
    }
    println!("\n─── Outcomes ────────────────────────────────────────────────────");
    for (name, count) in sorted_desc(&by_outcome) {
        println!("  {name:<20}  {count:>4}");
    }
}

/// Read the `data` object of every `SentinelHookInvocation` row.
fn hook_data_rows(conn: &Connection) -> Vec<serde_json::Value> {
    let pattern = format!("%\"type\":\"{NODE_HOOK}\"%");
    let Ok(mut stmt) = conn
        .prepare("SELECT payload FROM events WHERE type = 'object.created' AND payload LIKE ?1")
    else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![pattern], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.filter_map(std::result::Result::ok)
        .filter_map(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter_map(|p| p.get("object").and_then(|o| o.get("data")).cloned())
        .collect()
}

fn field_or(v: &serde_json::Value, key: &str) -> String {
    let s = str_field(v, key);
    if s.is_empty() { "?".to_string() } else { s }
}

/// Sort a count map by count descending, then name ascending for stability.
fn sorted_desc(map: &BTreeMap<String, i64>) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = map.iter().map(|(k, &c)| (k.clone(), c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // attach() creates the schema.
        conn
    }

    fn object_rows(conn: &Connection, otype: &str) -> Vec<serde_json::Value> {
        let pattern = format!("%\"type\":\"{otype}\"%");
        let mut stmt = conn
            .prepare("SELECT payload FROM events WHERE type = 'object.created' AND payload LIKE ?1")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![pattern], |r| r.get::<_, String>(0))
            .unwrap();
        rows.filter_map(std::result::Result::ok)
            .map(|t| serde_json::from_str(&t).unwrap())
            .collect()
    }

    fn event_rows(conn: &Connection, kind: &str) -> Vec<(Option<String>, String, serde_json::Value)> {
        let mut stmt = conn
            .prepare("SELECT actor, timestamp, payload FROM events WHERE type = ?1")
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![kind], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap();
        rows.filter_map(std::result::Result::ok)
            .map(|(a, ts, p)| (a, ts, serde_json::from_str(&p).unwrap()))
            .collect()
    }

    #[test]
    fn hook_dedupe_key_is_composite_not_trace_id_alone() {
        let shared_trace = "trace-1";
        let a = serde_json::json!({ "trace_id": shared_trace, "hook": "git_hygiene", "event": "PreToolUse", "ts": "t1" });
        let b = serde_json::json!({ "trace_id": shared_trace, "hook": "phase_gate", "event": "PreToolUse", "ts": "t1" });
        let c = serde_json::json!({ "trace_id": shared_trace, "hook": "git_hygiene", "event": "PreToolUse", "ts": "t2" });
        // Same trace_id but different hook → distinct keys (trace_id alone would collapse these).
        assert_ne!(hook_key(&a), hook_key(&b));
        // Same trace_id + hook + event but different ts → distinct.
        assert_ne!(hook_key(&a), hook_key(&c));
        // Identical → equal.
        assert_eq!(hook_key(&a), hook_key(&a.clone()));
    }

    #[test]
    fn dedupe_skips_identical_hooks_within_a_run() {
        let conn = mem();
        let mut ing = Ingestor::attach(&conn).unwrap();
        let h = serde_json::json!({
            "trace_id": "t", "hook": "h", "event": "PreToolUse", "ts": "ts1",
            "outcome": "allow", "session_id": "s1"
        });
        let added = ing.ingest_hooks(&[h.clone(), h]).unwrap();
        assert_eq!(added, 1, "identical hooks should dedupe to one");
        assert_eq!(ing.count_objects(NODE_HOOK), 1);
    }

    #[test]
    fn session_object_row_shape() {
        let conn = mem();
        let mut ing = Ingestor::attach(&conn).unwrap();
        ing.ingest_sessions(&[serde_json::json!({
            "session_id": "sess-a", "cwd": "/tmp/x", "platform": "windows", "ts": "2026-05-26T00:00:00Z"
        })])
        .unwrap();

        let rows = object_rows(&conn, NODE_SESSION);
        assert_eq!(rows.len(), 1);
        let obj = &rows[0]["object"];
        assert_eq!(obj["id"], "SentinelSession#sess-a");
        assert_eq!(obj["type"], "SentinelSession");
        assert_eq!(obj["data"]["session_id"], "sess-a");
        assert_eq!(obj["data"]["cwd"], "/tmp/x");
        assert_eq!(obj["data"]["platform"], "windows");
        assert_eq!(obj["data"]["started_at"], "2026-05-26T00:00:00Z");
    }

    #[test]
    fn hook_and_relation_row_shapes() {
        let conn = mem();
        let mut ing = Ingestor::attach(&conn).unwrap();
        ing.ingest_sessions(&[serde_json::json!({ "session_id": "s1", "ts": "t0" })]).unwrap();
        ing.ingest_hooks(&[serde_json::json!({
            "trace_id": "tr", "hook": "git_hygiene", "event": "PreToolUse", "ts": "t1",
            "outcome": "allow", "session_id": "s1", "duration_us": 42, "repo_root": "/repo"
        })])
        .unwrap();

        let hooks = object_rows(&conn, NODE_HOOK);
        assert_eq!(hooks.len(), 1);
        let data = &hooks[0]["object"]["data"];
        assert_eq!(data["hook"], "git_hygiene");
        assert_eq!(data["event"], "PreToolUse");
        assert_eq!(data["outcome"], "allow");
        assert_eq!(data["session_id"], "s1");
        assert_eq!(data["trace_id"], "tr");
        assert_eq!(data["duration_us"], 42);
        assert_eq!(data["repo_root"], "/repo");
        assert_eq!(data["ts"], "t1");

        let hook_id = hooks[0]["object"]["id"].as_str().unwrap().to_string();
        let rels = event_rows(&conn, "relation.created");
        assert_eq!(rels.len(), 1);
        let rel = &rels[0].2["relation"];
        assert_eq!(rel["source"], "SentinelSession#s1");
        assert_eq!(rel["target"], hook_id);
        assert_eq!(rel["type"], "has_invocation");
    }

    #[test]
    fn unknown_session_id_gets_a_stub_session() {
        let conn = mem();
        let mut ing = Ingestor::attach(&conn).unwrap();
        // No sessions ingested; the hook references an unseen session_id.
        ing.ingest_hooks(&[serde_json::json!({
            "trace_id": "tr", "hook": "h", "event": "Stop", "ts": "t1",
            "outcome": "allow", "session_id": "ghost"
        })])
        .unwrap();
        let sessions = object_rows(&conn, NODE_SESSION);
        assert_eq!(sessions.len(), 1, "a stub session should be created");
        assert_eq!(sessions[0]["object"]["data"]["session_id"], "ghost");
        assert_eq!(sessions[0]["object"]["data"]["cwd"], "", "stub has empty cwd");
    }

    #[test]
    fn domain_events_shapes_and_empty_actor_ts() {
        let conn = mem();
        let mut ing = Ingestor::attach(&conn).unwrap();
        ing.ingest_sessions(&[serde_json::json!({ "session_id": "s1", "ts": "t0" })]).unwrap();
        ing.ingest_hooks(&[
            serde_json::json!({ "trace_id": "a", "hook": "h1", "event": "PreToolUse", "ts": "t1", "outcome": "deny", "session_id": "s1" }),
            serde_json::json!({ "trace_id": "b", "hook": "h2", "event": "Stop", "ts": "t2", "outcome": "allow", "session_id": "s1" }),
        ])
        .unwrap();

        // session_started: actor NULL, timestamp "", payload carries ts.
        let started = event_rows(&conn, "sentinel.session_started");
        assert_eq!(started.len(), 1);
        assert!(started[0].0.is_none(), "sentinel.* actor must be NULL");
        assert_eq!(started[0].1, "", "sentinel.* SQL timestamp must be empty");
        assert_eq!(started[0].2["session_id"], "s1");
        assert_eq!(started[0].2["ts"], "t0");

        let ingested = event_rows(&conn, "sentinel.hook_ingested");
        assert_eq!(ingested.len(), 2);
        assert!(ingested.iter().all(|(a, ts, _)| a.is_none() && ts.is_empty()));
        assert!(ingested.iter().any(|(_, _, p)| p["sentinel_event"] == "PreToolUse" && p["outcome"] == "deny"));

        // hook_denied only emitted for the deny outcome.
        let denied = event_rows(&conn, "sentinel.hook_denied");
        assert_eq!(denied.len(), 1, "only the deny outcome emits hook_denied");
        assert_eq!(denied[0].2["hook"], "h1");
        assert_eq!(denied[0].2["session_id"], "s1");
    }

    #[test]
    fn resume_seeds_dedupe_from_existing_rows() {
        let conn = mem();
        {
            let mut ing = Ingestor::attach(&conn).unwrap();
            ing.ingest_hooks(&[serde_json::json!({
                "trace_id": "t", "hook": "h", "event": "PreToolUse", "ts": "t1",
                "outcome": "allow", "session_id": "s1"
            })])
            .unwrap();
        }
        // Re-attach (simulating a resumed import) and feed the same hook again.
        let mut ing2 = Ingestor::attach(&conn).unwrap();
        let added = ing2
            .ingest_hooks(&[serde_json::json!({
                "trace_id": "t", "hook": "h", "event": "PreToolUse", "ts": "t1",
                "outcome": "allow", "session_id": "s1"
            })])
            .unwrap();
        assert_eq!(added, 0, "resume must dedupe against rows already in the store");
        assert_eq!(ing2.count_objects(NODE_HOOK), 1);
    }

    #[test]
    fn read_new_resets_offset_on_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.jsonl");
        std::fs::write(&path, "line-1\n").unwrap();
        let size1 = file_size(&path);

        // Append; reading from size1 yields only the new tail.
        std::fs::write(&path, "line-1\nline-2\n").unwrap();
        let (delta, size2) = read_new(&path, size1).expect("new bytes after append");
        assert_eq!(delta, "line-2\n");
        assert!(size2 > size1);

        // Nothing new at the current offset.
        assert!(read_new(&path, size2).is_none());

        // Rotation: file shrinks below the remembered offset → re-read from 0.
        std::fs::write(&path, "fresh\n").unwrap();
        let (reread, size3) = read_new(&path, size2).expect("rotation should re-read from byte 0");
        assert_eq!(reread, "fresh\n", "rotation must reset offset and re-read whole file");
        assert_eq!(size3, file_size(&path));
    }
}
