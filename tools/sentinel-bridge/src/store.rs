//! SQLite writer that mimics the activegraph schema 1:1.
//!
//! The Rust viz-api (`tools/sentinel-viz-api`) reads this store. Any
//! deviation from the on-disk format the Python `sentinel_bridge.py`
//! produced will break the read path. The schema, payload shapes,
//! and ID conventions are documented below and replicated here.
//!
//! Schema (`schema_version=1`):
//!   events(seq INTEGER PK AUTOINCREMENT, id TEXT, type TEXT,
//!          actor TEXT, payload TEXT, frame_id TEXT, caused_by TEXT,
//!          timestamp TEXT, run_id TEXT, UNIQUE(id, run_id))
//!   runs(run_id PK, parent_run_id, forked_at_event_id, label,
//!        created_at, goal, frame_id)
//!   meta(key PK, value)
//!
//! IDs:
//!   - event.id:    evt_001, evt_002 ...  (zero-padded to 3, then natural)
//!   - object.id:   `<Type>#N`  (e.g. SentinelSession#42)
//!   - relation.id: rel_001, rel_002 ...
//!
//! Payload shape for object.created (matches Python output verbatim):
//!   { "object": { "id", "type", "data": {..}, "version": 1,
//!                 "provenance": { "created_by": "sentinel_bridge",
//!                                 "caused_by_event": null,
//!                                 "frame_id": null,
//!                                 "timestamp": "<ISO8601>",
//!                                 "evidence": [],
//!                                 "run_id": "<RUN>" } },
//!     "id": "<same as object.id>" }
//!
//! For relation.created the outer keys include redundant copies of
//! source / target / id (the Python writer does this; we mirror it).

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

const SCHEMA_VERSION: &str = "1";
pub const ACTOR: &str = "sentinel_bridge";

const DDL: &str = "
CREATE TABLE IF NOT EXISTS events (
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
CREATE INDEX IF NOT EXISTS idx_events_type ON events(type);

CREATE TABLE IF NOT EXISTS runs (
    run_id TEXT PRIMARY KEY,
    parent_run_id TEXT,
    forked_at_event_id TEXT,
    label TEXT,
    created_at TEXT NOT NULL,
    goal TEXT,
    frame_id TEXT
);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// One persistent bridge store. Owns a SQLite connection and the
/// in-process ID counters for events / objects / relations.
pub struct Store {
    conn: Connection,
    run_id: String,
    event_counter: u64,
    rel_counter: u64,
    /// Per-object-type counters. SentinelSession → next N for new
    /// session objects, etc.
    obj_counters: std::collections::HashMap<String, u64>,
    /// In-memory dedup of already-ingested SentinelHookInvocation
    /// records, keyed by trace_id. Avoids a per-record SQL query
    /// over a multi-hundred-thousand-row events table. Mirrors the
    /// `seen_traces` set the Python bridge built at startup.
    seen_traces: std::collections::HashSet<String>,
    /// session_id → SentinelSession#N (object id). Avoids the same
    /// per-record SQL pattern when materialising hook → session
    /// relations.
    session_index: std::collections::HashMap<String, String>,
}

impl Store {
    /// Open (or create) the store. Initialises schema on first use.
    /// On reopen, picks up existing ID counters so new writes don't
    /// collide with already-persisted rows.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        // WAL mode matches what activegraph does — better tail
        // concurrency with the viz-api reader.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(DDL)?;

        // Pin schema version once.
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?)",
            params![SCHEMA_VERSION],
        )?;

        // Restore ID counters from the existing rows so new evt_/rel_
        // IDs don't collide.
        let event_counter = max_event_seq(&conn)?;
        let rel_counter = max_rel_seq(&conn)?;
        let obj_counters = restore_obj_counters(&conn)?;

        // Seed in-memory caches so we don't re-query SQL on every
        // record. Matters on first-pass backfill with O(100k+) hooks.
        let seen_traces = load_seen_traces(&conn)?;
        let session_index = load_session_index(&conn)?;

        let run_id = ulid::Ulid::new().to_string();
        Self::insert_run(&conn, &run_id)?;

        Ok(Self {
            conn,
            run_id,
            event_counter,
            rel_counter,
            obj_counters,
            seen_traces,
            session_index,
        })
    }

    fn insert_run(conn: &Connection, run_id: &str) -> Result<()> {
        let created_at = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        conn.execute(
            "INSERT OR IGNORE INTO runs(run_id, created_at) VALUES (?, ?)",
            params![run_id, created_at],
        )?;
        Ok(())
    }

    /// Look up the existing object.id for a SentinelSession with the
    /// given session_id (the bridge keys sessions by that). Returns
    /// None if no such session has been ingested in this run or any
    /// prior run. Reads from the in-memory session_index cache
    /// (seeded from SQL at startup).
    pub fn lookup_session_obj_id(&self, session_id: &str) -> Result<Option<String>> {
        Ok(self.session_index.get(session_id).cloned())
    }

    /// Has a SentinelHookInvocation with this trace_id already been
    /// ingested? In-memory lookup against the seen_traces set.
    pub fn hook_trace_exists(&self, trace_id: &str) -> Result<bool> {
        Ok(self.seen_traces.contains(trace_id))
    }

    fn next_event_id(&mut self) -> String {
        self.event_counter += 1;
        format!("evt_{:03}", self.event_counter)
    }

    fn next_rel_id(&mut self) -> String {
        self.rel_counter += 1;
        format!("rel_{:03}", self.rel_counter)
    }

    fn next_obj_id(&mut self, obj_type: &str) -> String {
        let n = self.obj_counters.entry(obj_type.to_string()).or_insert(0);
        *n += 1;
        format!("{obj_type}#{}", *n)
    }

    fn provenance(&self, ts: &str) -> Value {
        json!({
            "created_by": ACTOR,
            "caused_by_event": Value::Null,
            "frame_id": Value::Null,
            "timestamp": ts,
            "evidence": [],
            "run_id": self.run_id,
        })
    }

    fn write_event(
        &mut self,
        event_type: &str,
        payload: &Value,
        ts: &str,
    ) -> Result<String> {
        let event_id = self.next_event_id();
        self.conn.execute(
            "INSERT INTO events(id, type, actor, payload, timestamp, run_id)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                event_id,
                event_type,
                ACTOR,
                serde_json::to_string(payload)?,
                ts,
                self.run_id,
            ],
        )?;
        Ok(event_id)
    }

    /// Materialise a SentinelSession object iff a session with this
    /// session_id has not been seen. Returns the object.id (newly
    /// created or pre-existing).
    pub fn upsert_session(&mut self, sd: &SessionData) -> Result<String> {
        if let Some(existing) = self.lookup_session_obj_id(&sd.session_id)? {
            return Ok(existing);
        }
        let now_iso = now_iso();
        let obj_id = self.next_obj_id("SentinelSession");
        self.session_index
            .insert(sd.session_id.clone(), obj_id.clone());
        let provenance = self.provenance(&now_iso);
        let payload = json!({
            "object": {
                "id": obj_id,
                "type": "SentinelSession",
                "data": {
                    "session_id": sd.session_id,
                    "cwd": sd.cwd,
                    "platform": sd.platform,
                    "started_at": sd.started_at,
                    "source_harness": sd.source_harness,
                },
                "version": 1,
                "provenance": provenance,
            },
            "id": obj_id,
        });
        self.write_event("object.created", &payload, &now_iso)?;
        // Domain event: a session started — preserves the
        // sentinel.session_started event the Python bridge emitted,
        // but ONLY for native-claude sessions where a real
        // sessions.jsonl session_start record drove the creation.
        // The Python bridge only ever emitted this for ~73 sessions
        // (all claude); shim-derived sessions emitting it floods the
        // ticker with thousands of empty "session_started" rows.
        if sd.source_harness == "claude" {
            let dom = json!({
                "session_id": sd.session_id,
                "ts": sd.started_at,
            });
            self.write_event("sentinel.session_started", &dom, &now_iso)?;
        }
        Ok(obj_id)
    }

    /// Materialise a SentinelHookInvocation + has_invocation relation
    /// + sentinel.hook_ingested event. Idempotent via trace_id.
    pub fn ingest_hook(&mut self, hi: &HookData, session_obj_id: &str) -> Result<()> {
        if self.hook_trace_exists(&hi.trace_id)? {
            return Ok(());
        }
        self.seen_traces.insert(hi.trace_id.clone());
        let now_iso = now_iso();
        let obj_id = self.next_obj_id("SentinelHookInvocation");
        let provenance = self.provenance(&now_iso);
        let obj_payload = json!({
            "object": {
                "id": obj_id,
                "type": "SentinelHookInvocation",
                "data": {
                    "hook": hi.hook,
                    "event": hi.event,
                    "outcome": hi.outcome,
                    "session_id": hi.session_id,
                    "trace_id": hi.trace_id,
                    "duration_us": hi.duration_us,
                    "repo_root": hi.repo_root,
                    "ts": hi.ts,
                    "source_harness": hi.source_harness,
                    "tool": hi.tool,
                },
                "version": 1,
                "provenance": provenance.clone(),
            },
            "id": obj_id,
        });
        self.write_event("object.created", &obj_payload, &now_iso)?;

        // has_invocation relation: Session → HookInvocation
        let rel_id = self.next_rel_id();
        let rel_payload = json!({
            "relation": {
                "id": rel_id,
                "source": session_obj_id,
                "target": obj_id,
                "type": "has_invocation",
                "data": {},
                "provenance": provenance,
            },
            "id": rel_id,
            "source": session_obj_id,
            "target": obj_id,
        });
        self.write_event("relation.created", &rel_payload, &now_iso)?;

        // Domain event the viz-api ticker tail consumes.
        // Includes `tool` and `source_harness` so the dashboard can
        // categorize tool calls + tag each session strip without
        // needing the (often out-of-window) SentinelSession node.
        let dom = json!({
            "hook": hi.hook,
            "sentinel_event": hi.event,
            "outcome": hi.outcome,
            "session_id": hi.session_id,
            "trace_id": hi.trace_id,
            "duration_us": hi.duration_us,
            "ts": hi.ts,
            "tool": hi.tool,
            "source_harness": hi.source_harness,
        });
        self.write_event("sentinel.hook_ingested", &dom, &now_iso)?;

        // Deny callouts get a dedicated event for alerting behaviours.
        if hi.outcome == "deny" {
            let deny = json!({
                "hook": hi.hook,
                "sentinel_event": hi.event,
                "session_id": hi.session_id,
                "trace_id": hi.trace_id,
                "ts": hi.ts,
            });
            self.write_event("sentinel.hook_denied", &deny, &now_iso)?;
        }
        Ok(())
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Begin a transaction. The bridge wraps each ingest pass in one
    /// so a few hundred thousand inserts don't pay per-row fsync.
    pub fn begin(&mut self) -> Result<()> {
        self.conn.execute("BEGIN", [])?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }
}

fn max_event_seq(conn: &Connection) -> Result<u64> {
    // event.id is `evt_NNN` — pull the max numeric suffix to seed
    // our counter so new writes don't collide with persisted rows.
    let mut stmt = conn.prepare("SELECT id FROM events WHERE id LIKE 'evt_%'")?;
    let mut max_seen: u64 = 0;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if let Some(rest) = id.strip_prefix("evt_") {
            if let Ok(n) = rest.parse::<u64>() {
                max_seen = max_seen.max(n);
            }
        }
    }
    Ok(max_seen)
}

fn max_rel_seq(conn: &Connection) -> Result<u64> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload, '$.relation.id')
         FROM events WHERE type='relation.created'",
    )?;
    let mut max_seen: u64 = 0;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let id: Option<String> = row.get(0)?;
        if let Some(id) = id {
            if let Some(rest) = id.strip_prefix("rel_") {
                if let Ok(n) = rest.parse::<u64>() {
                    max_seen = max_seen.max(n);
                }
            }
        }
    }
    Ok(max_seen)
}

fn restore_obj_counters(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, u64>> {
    let mut out: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload, '$.object.type'),
                json_extract(payload, '$.object.id')
         FROM events WHERE type='object.created'",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let ty: Option<String> = row.get(0)?;
        let id: Option<String> = row.get(1)?;
        if let (Some(ty), Some(id)) = (ty, id) {
            if let Some(rest) = id.strip_prefix(&format!("{ty}#")) {
                if let Ok(n) = rest.parse::<u64>() {
                    let entry = out.entry(ty).or_insert(0);
                    if n > *entry {
                        *entry = n;
                    }
                }
            }
        }
    }
    Ok(out)
}

fn load_seen_traces(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload, '$.object.data.trace_id')
         FROM events
         WHERE type='object.created'
           AND json_extract(payload, '$.object.type') = 'SentinelHookInvocation'",
    )?;
    let mut out = std::collections::HashSet::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let tid: Option<String> = row.get(0)?;
        if let Some(t) = tid {
            out.insert(t);
        }
    }
    Ok(out)
}

fn load_session_index(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, String>> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload, '$.object.data.session_id'),
                json_extract(payload, '$.object.id')
         FROM events
         WHERE type='object.created'
           AND json_extract(payload, '$.object.type') = 'SentinelSession'",
    )?;
    let mut out = std::collections::HashMap::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let sid: Option<String> = row.get(0)?;
        let oid: Option<String> = row.get(1)?;
        if let (Some(sid), Some(oid)) = (sid, oid) {
            out.entry(sid).or_insert(oid);
        }
    }
    Ok(out)
}

pub fn now_iso() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// ─── Input payloads ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionData {
    pub session_id: String,
    pub cwd: String,
    pub platform: String,
    pub started_at: String,
    pub source_harness: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookData {
    pub hook: String,
    pub event: String,
    pub outcome: String,
    pub session_id: String,
    pub trace_id: String,
    pub duration_us: u64,
    pub repo_root: String,
    pub ts: String,
    pub source_harness: String,
    /// Claude-normalized tool name (Bash / Read / Edit / Write / etc.).
    /// Per-harness shims map their native tool name onto this set so
    /// the dashboard's categorizer doesn't have to know about each
    /// harness's local taxonomy. Empty string = no tool (e.g.
    /// UserPromptSubmit, SessionStart).
    #[serde(default)]
    pub tool: String,
}
