//! SQLite writer for the viz read-model.
//!
//! Relational schema (schema_version=2). Replaces the old
//! "activegraph-compatible" event-sourced store (events/object.created/
//! relation.created/runs) — see plans/sentinel-viz-yeet-activegraph.md.
//! The viz-api (`tools/sentinel-viz-api`) reads these tables read-only.
//! The JSONL metric files remain the durable source of truth; this
//! SQLite is a disposable derived view that can be rebuilt from JSONL
//! at any time (`sentinel-bridge backfill` against a fresh DB).
//!
//! Schema:
//!   sessions(session_id PK, source_harness, cwd, platform,
//!            started_at, last_activity_ts)
//!   hook_events(id PK AUTOINCREMENT, session_id, ts, sentinel_event,
//!               hook, tool, outcome, duration_us, trace_id UNIQUE,
//!               source_harness)
//!   meta(key PK, value)

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

const SCHEMA_VERSION: &str = "2";

const DDL: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    session_id       TEXT PRIMARY KEY,
    source_harness   TEXT NOT NULL DEFAULT 'claude',
    cwd              TEXT NOT NULL DEFAULT '',
    platform         TEXT NOT NULL DEFAULT '',
    started_at       TEXT NOT NULL DEFAULT '',
    last_activity_ts TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_sessions_activity ON sessions(last_activity_ts);

CREATE TABLE IF NOT EXISTS hook_events (
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
CREATE INDEX IF NOT EXISTS idx_hook_session_ts ON hook_events(session_id, ts);

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// One persistent bridge store. Owns a SQLite connection and in-memory
/// caches that avoid per-record SQL on a hot backfill.
pub struct Store {
    conn: Connection,
    run_id: String,
    /// session_ids already materialised (this run or a prior one).
    known_sessions: HashSet<String>,
    /// trace_ids already ingested — idempotency for hook_events.
    seen_traces: HashSet<String>,
}

impl Store {
    /// Open (or create) the store. Initialises schema and seeds the
    /// in-memory caches from existing rows so reopen doesn't re-insert.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(DDL)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?)",
            params![SCHEMA_VERSION],
        )?;

        let known_sessions = load_known_sessions(&conn)?;
        let seen_traces = load_seen_traces(&conn)?;
        let run_id = ulid::Ulid::new().to_string();

        Ok(Self { conn, run_id, known_sessions, seen_traces })
    }

    /// Insert a session row if absent. Idempotent.
    pub fn upsert_session(&mut self, sd: &SessionData) -> Result<()> {
        if self.known_sessions.contains(&sd.session_id) {
            return Ok(());
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO sessions
             (session_id, source_harness, cwd, platform, started_at, last_activity_ts)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                sd.session_id,
                sd.source_harness,
                sd.cwd,
                sd.platform,
                sd.started_at,
                sd.started_at, // seed last_activity from start; hooks bump it
            ],
        )?;
        self.known_sessions.insert(sd.session_id.clone());
        Ok(())
    }

    /// Insert a hook_event (idempotent via trace_id), creating a stub
    /// session first if the session_id hasn't been seen, and advancing
    /// the session's last_activity_ts when this hook is newer.
    pub fn ingest_hook(&mut self, hi: &HookData) -> Result<()> {
        if self.seen_traces.contains(&hi.trace_id) {
            return Ok(());
        }
        if !self.known_sessions.contains(&hi.session_id) {
            // Stub session from the hook's own metadata — this is how
            // codex/all-harness sessions (no sessions.jsonl) exist.
            let stub = SessionData {
                session_id: hi.session_id.clone(),
                cwd: hi.repo_root.clone(),
                platform: String::new(),
                started_at: hi.ts.clone(),
                source_harness: hi.source_harness.clone(),
            };
            self.upsert_session(&stub)?;
        }

        self.conn.execute(
            "INSERT OR IGNORE INTO hook_events
             (session_id, ts, sentinel_event, hook, tool, outcome, duration_us, trace_id, source_harness)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                hi.session_id,
                hi.ts,
                hi.event,
                hi.hook,
                hi.tool,
                hi.outcome,
                hi.duration_us,
                hi.trace_id,
                hi.source_harness,
            ],
        )?;
        self.seen_traces.insert(hi.trace_id.clone());

        // Advance last_activity_ts when this hook is newer. ISO8601 in
        // UTC sorts lexicographically, so a string MAX is correct.
        self.conn.execute(
            "UPDATE sessions SET last_activity_ts = ?
             WHERE session_id = ? AND ? > last_activity_ts",
            params![hi.ts, hi.session_id, hi.ts],
        )?;
        Ok(())
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Wrap an ingest pass in one transaction so a backfill of
    /// hundreds of thousands of rows doesn't pay per-row fsync.
    pub fn begin(&mut self) -> Result<()> {
        self.conn.execute("BEGIN", [])?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.conn.execute("COMMIT", [])?;
        Ok(())
    }
}

fn load_known_sessions(conn: &Connection) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    // Table may not exist yet on first open of a brand-new file; DDL
    // ran already so this is safe.
    let mut stmt = conn.prepare("SELECT session_id FROM sessions")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        out.insert(row.get::<_, String>(0)?);
    }
    Ok(out)
}

fn load_seen_traces(conn: &Connection) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let mut stmt = conn.prepare("SELECT trace_id FROM hook_events")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        out.insert(row.get::<_, String>(0)?);
    }
    Ok(out)
}

#[allow(dead_code)]
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
    /// Empty string = no tool (UserPromptSubmit, SessionStart).
    #[serde(default)]
    pub tool: String,
}