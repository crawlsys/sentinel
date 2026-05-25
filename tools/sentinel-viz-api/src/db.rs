use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

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

/// Read every event ordered by `seq` ascending.
pub fn read_events(conn: &Connection) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT seq, id, type, actor, payload, frame_id, caused_by, timestamp, run_id \
         FROM events ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([], |row| {
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
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}
