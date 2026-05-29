use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

/// Default location of the bridge's SQLite store, overridable via env.
///
/// WORKSTREAM: sentinel-bridge — this path is owned by
/// `tools/sentinel-bridge`, which writes the file (relational schema:
/// `sessions` + `hook_events`). The viz crate opens READ-ONLY. If the
/// bridge moves, override `SENTINEL_VIZ_DB` rather than hard-coding a
/// new default here.
pub const DEFAULT_DB_ENV: &str = "SENTINEL_VIZ_DB";
const DEFAULT_DB_REL: &str = ".agents/scratch/activegraph-bridge/sentinel.db";

/// How many recent sessions the dashboard renders. Recency is by
/// `last_activity_ts`, NOT by row insertion order — this is what makes
/// a long-quiet-then-active session reappear, and removes the old
/// hidden top-K (=5) truncation.
pub const MAX_SESSIONS: usize = 200;

/// Resolve the SQLite path from `$SENTINEL_VIZ_DB`, falling back to
/// `$HOME/.agents/scratch/activegraph-bridge/sentinel.db`.
pub fn default_db_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var(DEFAULT_DB_ENV) {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().context("could not determine $HOME")?;
    Ok(home.join(DEFAULT_DB_REL))
}

/// Open the bridge SQLite store read-only. The bridge owns writes.
pub fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening sentinel db at {}", path.display()))
}

/// Cheap recency probe — `MAX(id)` over `hook_events`, used by SSE/health
/// to decide whether a full graph reload is needed. (Name retained from
/// the event-store era; the value is now the newest hook_event rowid.)
pub fn peek_max_seq(conn: &Connection) -> Result<i64> {
    let v: Option<i64> = conn
        .query_row("SELECT MAX(id) FROM hook_events", [], |row| row.get(0))
        .unwrap_or(None);
    Ok(v.unwrap_or(0))
}

/// One row of the `sessions` table.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub source_harness: String,
    pub cwd: String,
    pub platform: String,
    pub started_at: String,
    pub last_activity_ts: String,
}

/// One row of the `hook_events` table.
#[derive(Debug, Clone)]
pub struct HookEventRow {
    pub id: i64,
    pub session_id: String,
    pub ts: String,
    pub sentinel_event: String,
    pub hook: String,
    pub tool: String,
    pub outcome: String,
    pub duration_us: i64,
    pub source_harness: String,
}

/// Read the most-recently-active sessions. `since_secs` is a coarse
/// time floor on `last_activity_ts` (lexicographic on ISO-8601 UTC —
/// fine for a window cutoff and index-friendly). Ordered newest-active
/// first, capped at `limit`.
fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        session_id: row.get(0)?,
        source_harness: row.get(1)?,
        cwd: row.get(2)?,
        platform: row.get(3)?,
        started_at: row.get(4)?,
        last_activity_ts: row.get(5)?,
    })
}

const SESSION_COLS: &str =
    "session_id, source_harness, cwd, platform, started_at, last_activity_ts";

/// Fetch a single session by id (honours `?focus=` even when the
/// session is older than the recent-window cutoff).
pub fn read_session(conn: &Connection, session_id: &str) -> Result<Option<SessionRow>> {
    let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE session_id = ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map([session_id], session_from_row)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

pub fn read_recent_sessions(
    conn: &Connection,
    since_secs: Option<i64>,
    limit: usize,
) -> Result<Vec<SessionRow>> {
    let map_row = session_from_row;
    let cols = SESSION_COLS;
    let mut out = Vec::new();
    match since_secs {
        Some(secs) => {
            let cutoff = cutoff_iso(secs);
            let sql = format!(
                "SELECT {cols} FROM sessions \
                 WHERE last_activity_ts >= ?1 \
                 ORDER BY last_activity_ts DESC LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![cutoff, limit as i64], map_row)?;
            for r in rows {
                out.push(r?);
            }
        }
        None => {
            let sql = format!(
                "SELECT {cols} FROM sessions ORDER BY last_activity_ts DESC LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![limit as i64], map_row)?;
            for r in rows {
                out.push(r?);
            }
        }
    }
    Ok(out)
}

/// Read the newest `limit` hook_events globally, newest-first (by
/// rowid). Walks the integer-PK index descending and stops after
/// `limit` rows — O(limit), no sort, regardless of table size. The
/// caller filters to the sessions it is displaying in memory.
///
/// Why global rather than `WHERE session_id IN (...)`: an IN-filter
/// combined with `ORDER BY id DESC` cannot use the PK for ordering, so
/// SQLite gathers every matching row and sorts it (≈1.2s on 100k rows).
/// Recent-by-activity sessions own the newest events anyway, so the
/// global tail covers them; there is NO hidden per-session cap.
pub fn read_recent_events(conn: &Connection, limit: usize) -> Result<Vec<HookEventRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let sql = "SELECT id, session_id, ts, sentinel_event, hook, tool, outcome, duration_us, source_harness \
               FROM hook_events ORDER BY id DESC LIMIT ?1";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok(HookEventRow {
            id: row.get(0)?,
            session_id: row.get(1)?,
            ts: row.get(2)?,
            sentinel_event: row.get(3)?,
            hook: row.get(4)?,
            tool: row.get(5)?,
            outcome: row.get(6)?,
            duration_us: row.get(7)?,
            source_harness: row.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// ISO-8601 UTC string for `now - secs`, matching the bridge's stored
/// timestamp format closely enough for a lexicographic floor.
fn cutoff_iso(secs: i64) -> String {
    let t = chrono::Utc::now() - chrono::Duration::seconds(secs);
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
