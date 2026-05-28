use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, Row};

use crate::model::Event;

/// Default location of the bridge's SQLite store, overridable via env.
///
/// WORKSTREAM: sentinel-bridge — this path is owned by
/// `tools/sentinel-viz/sentinel_bridge.py`, which writes the file.
/// The viz crate opens READ-ONLY. If the bridge moves, override
/// `SENTINEL_VIZ_DB` rather than hard-coding a new default here.
pub const DEFAULT_DB_ENV: &str = "SENTINEL_VIZ_DB";
const DEFAULT_DB_REL: &str = ".agents/scratch/activegraph-bridge/sentinel.db";

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

/// Cheap MAX(seq) probe — single index hit, used by SSE to decide
/// whether a full graph reload is needed.
pub fn peek_max_seq(conn: &Connection) -> Result<i64> {
    let seq: Option<i64> = conn
        .query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))
        .unwrap_or(None);
    Ok(seq.unwrap_or(0))
}

/// Read the recent event window ordered by `seq` ascending.
///
/// The dashboard only renders a bounded live window, but the bridge DB can
/// grow past a million rows during harness/shim demos. Full scans made first
/// render block behind JSON payload parsing. Keep the historical DB intact and
/// read only the newest rows by default; set `SENTINEL_VIZ_EVENT_WINDOW_ROWS=0`
/// to restore full-corpus reads for offline analysis.
pub fn read_events(conn: &Connection) -> Result<Vec<Event>> {
    let window_rows = std::env::var("SENTINEL_VIZ_EVENT_WINDOW_ROWS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(30_000);
    let max_seq = peek_max_seq(conn)?;
    let min_seq = if window_rows <= 0 {
        0
    } else {
        (max_seq - window_rows + 1).max(0)
    };
    read_events_from_seq(conn, min_seq)
}

/// Explicit full-corpus read for offline tests/analysis.
pub fn read_all_events(conn: &Connection) -> Result<Vec<Event>> {
    read_events_from_seq(conn, 0)
}

fn read_events_from_seq(conn: &Connection, min_seq: i64) -> Result<Vec<Event>> {
    let sql = if min_seq > 0 {
        "SELECT seq, id, type, actor, payload, frame_id, caused_by, timestamp, run_id \
         FROM events WHERE seq >= ?1 ORDER BY seq ASC"
    } else {
        "SELECT seq, id, type, actor, payload, frame_id, caused_by, timestamp, run_id \
         FROM events ORDER BY seq ASC"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    if min_seq > 0 {
        let rows = stmt.query_map([min_seq], row_to_event)?;
        for r in rows {
            out.push(r?);
        }
    } else {
        let rows = stmt.query_map([], row_to_event)?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

fn row_to_event(row: &Row<'_>) -> rusqlite::Result<Event> {
    let payload_text: String = row.get(4)?;
    let payload: serde_json::Value =
        serde_json::from_str(&payload_text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;
    Ok(Event {
        seq: row.get(0)?,
        id: row.get(1)?,
        kind: row.get(2)?,
        actor: row.get(3)?,
        payload,
        frame_id: row.get(5)?,
        caused_by: row.get(6)?,
        timestamp: row.get(7)?,
        run_id: row.get(8)?,
    })
}
