//! Snapshot-style telemetry sources (LEG-261) — KPI scan summaries and the
//! agent $/issue usage rollup, shipped only when their content changes.
//!
//! Unlike the append-only ledgers ([`super::ledger`]) these sources are
//! *state*, regenerated or mutated in place. Each collect run:
//!
//! 1. builds the snapshot's canonical content (raw file bytes for the KPI
//!    summaries, a deterministic NDJSON rendering of the rollup queries for
//!    the sqlite sources),
//! 2. compares its sha256 against the [`SnapshotCursor`] persisted in the
//!    shared checkpoint — unchanged content spools **zero** objects,
//! 3. on change, stages one full snapshot batch into the same spool the
//!    ledger collector uses, then persists the cursor (write-then-persist,
//!    matching the LEG-259 crash-window contract).
//!
//! Capture stamps are derived from the *data* (file mtime for KPI files,
//! max activity timestamp for the sqlite rollups), never from wall-clock
//! run time — so a crash-window re-run reproduces an identical batch (same
//! key → overwrite), and every record carries `schema` + `captured_at`.
//!
//! Three streams, three key layouts (plan §3):
//!
//! - `v1/kpi/{scan}/dt={date}/{captured}_{sha12}.ndjson.zst` — the
//!   `sentinel … scan` outputs (`roi-summary.json` & friends), one record
//!   per summary (JSONL inputs ship one record per line);
//! - `v1/usage-by-issue/dt={date}/{captured}_{sha12}.ndjson.zst` — the
//!   **ticket-cost association stream**: one record per Linear issue
//!   bucket (incl. `unassigned`) mirroring ldesk's
//!   `usage_rollup_by_issue()` contract — resume runs sharing one
//!   `claude_session_id` are deduped to the most recent sample before
//!   summing, so session-cumulative usage is never double-counted;
//! - `v1/session-issue/dt={date}/{captured}_{sha12}.ndjson.zst` — the
//!   minimal `claude_session_id → linear_issue_id` map from `agent_runs`,
//!   letting a lake query join the LEG-260 hook ledgers (which carry
//!   `session_id`) to tickets.
//!
//! The ldesk database is opened **read-only** (`SQLITE_OPEN_READ_ONLY`,
//! WAL-safe) — collection works whether or not the app is running and can
//! never mutate it.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

use super::checkpoint::{CheckpointStore, SnapshotCursor};
use super::ledger::{compact_ts, date_of_ts, SourceStats, TelemetrySource};
use super::spool::{self, sha256_hex, BatchSpec, SpoolConfig};

/// Schema tag on every usage-by-issue record.
pub const USAGE_BY_ISSUE_SCHEMA: &str = "usage_by_issue.v1";

/// Schema tag on every session→issue mapping record.
pub const SESSION_ISSUE_SCHEMA: &str = "session_issue.v1";

/// Issue-bucket label for runs with no `linear_issue_id` (mirrors ldesk's
/// `IssueBucket::Unassigned`).
pub const UNASSIGNED_BUCKET: &str = "unassigned";

// ---------------------------------------------------------------------------
// Shared gate-and-spool helper
// ---------------------------------------------------------------------------

/// One fully-built snapshot, ready to gate against the checkpoint.
struct SnapshotBatch {
    /// sha256 over the *canonical* content (raw file bytes / rendered
    /// NDJSON) — the ship-on-change gate.
    gate_sha256: String,
    /// The NDJSON to spool when the gate opens.
    ndjson: Vec<u8>,
    rows: u64,
    /// RFC3339, derived from the data (mtime / max activity) — deterministic.
    captured_at: String,
    /// Key prefix base without the `dt=` partition, e.g. `v1/kpi/roi`.
    key_base: String,
    kind: String,
    schema: String,
}

/// Gate the snapshot on content hash, spool it when changed, persist the
/// cursor. Unchanged content is a no-op (zero objects, zero rows).
fn spool_if_changed(
    name: &str,
    batch: &SnapshotBatch,
    store: &mut CheckpointStore,
    spool: &SpoolConfig,
    stats: &mut SourceStats,
) -> Result<()> {
    if let Some(cursor) = store.snapshot(name) {
        if cursor.sha256 == batch.gate_sha256 {
            return Ok(()); // unchanged → zero objects
        }
    }

    let stamp = compact_ts(&batch.captured_at)
        .unwrap_or_else(|| batch.captured_at.replace([':', '-', '.'], ""));
    let dt = date_of_ts(&batch.captured_at)
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
    let spec = BatchSpec {
        source: name.to_string(),
        kind: batch.kind.clone(),
        schema: batch.schema.clone(),
        key_prefix: format!("{}/dt={dt}", batch.key_base),
        stamp,
        rows: batch.rows,
        first_ts: Some(batch.captured_at.clone()),
        last_ts: Some(batch.captured_at.clone()),
    };
    let manifest = spool::write_batch(spool, &spec, &batch.ndjson)?;
    stats.rows += batch.rows;
    stats.batches += 1;
    stats.spooled_bytes += manifest.compressed_bytes;

    // Persist the cursor only after the batch is durably spooled — a crash
    // in between re-produces the identical batch (deterministic stamp +
    // content hash → same file name → overwrite).
    store.set_snapshot(
        name,
        SnapshotCursor {
            sha256: batch.gate_sha256.clone(),
            captured_at: batch.captured_at.clone(),
        },
    );
    store.persist()
}

/// File mtime → RFC3339 UTC (the snapshot's deterministic capture stamp).
fn mtime_rfc3339(meta: &fs::Metadata) -> Result<String> {
    let mtime = meta.modified().context("file mtime unavailable")?;
    Ok(chrono::DateTime::<chrono::Utc>::from(mtime).to_rfc3339())
}

// ---------------------------------------------------------------------------
// (a) KPI scan summary files
// ---------------------------------------------------------------------------

/// How a KPI file's bytes become records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KpiFormat {
    /// One JSON document → one stamped record.
    Json,
    /// JSONL → one stamped record per line.
    Jsonl,
}

/// One `sentinel … scan` output file shipped as a content-hash-gated
/// snapshot (`v1/kpi/{scan}/…`).
#[derive(Debug, Clone)]
pub struct KpiFileSource {
    /// Source/checkpoint name, `kpi-{scan}`.
    name: String,
    /// Short scan name used in the object key (`roi`, `cache-efficiency`, …).
    pub scan: String,
    /// Per-record schema tag (`roi-summary.v1`, …).
    pub schema: String,
    pub path: PathBuf,
    pub format: KpiFormat,
}

impl KpiFileSource {
    #[must_use]
    pub fn new(scan: &str, schema: &str, path: PathBuf, format: KpiFormat) -> Self {
        Self {
            name: format!("kpi-{scan}"),
            scan: scan.to_string(),
            schema: schema.to_string(),
            path,
            format,
        }
    }
}

/// The four KPI scan outputs the report consumes (plan §1), under
/// sentinel's metrics dir (honors `SENTINEL_CLAUDE_DIR`).
#[must_use]
pub fn default_kpi_sources() -> Vec<KpiFileSource> {
    let metrics = crate::paths::claude_dir().join("sentinel").join("metrics");
    vec![
        KpiFileSource::new(
            "roi",
            "roi-summary.v1",
            metrics.join("roi-summary.json"),
            KpiFormat::Json,
        ),
        KpiFileSource::new(
            "cache-efficiency",
            "cache-efficiency-summary.v1",
            metrics.join("cache-efficiency-summary.json"),
            KpiFormat::Json,
        ),
        KpiFileSource::new(
            "cost-per-point",
            "cost-per-point-summary.v1",
            metrics.join("cost-per-point-summary.json"),
            KpiFormat::Json,
        ),
        KpiFileSource::new(
            "tokens-per-ticket",
            "tokens-per-ticket.v1",
            metrics.join("tokens-per-ticket.jsonl"),
            KpiFormat::Jsonl,
        ),
    ]
}

impl TelemetrySource for KpiFileSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn collect(&self, store: &mut CheckpointStore, spool: &SpoolConfig) -> Result<SourceStats> {
        let mut stats = SourceStats::default();
        if !self.path.exists() {
            stats
                .notes
                .push(format!("kpi snapshot absent: {}", self.path.display()));
            return Ok(stats);
        }
        let raw = fs::read(&self.path).with_context(|| format!("read {}", self.path.display()))?;
        if raw.iter().all(u8::is_ascii_whitespace) {
            stats
                .notes
                .push(format!("kpi snapshot empty: {}", self.path.display()));
            return Ok(stats);
        }
        let meta =
            fs::metadata(&self.path).with_context(|| format!("stat {}", self.path.display()))?;
        let captured_at = mtime_rfc3339(&meta)?;

        let (ndjson, rows) = render_kpi(&raw, self.format, &self.schema, &captured_at);
        let batch = SnapshotBatch {
            // Gate on the RAW file bytes: a scan that rewrites identical
            // content (new mtime, same bytes) must spool nothing.
            gate_sha256: sha256_hex(&raw),
            ndjson,
            rows,
            captured_at,
            key_base: format!("v1/kpi/{}", self.scan),
            kind: "kpi".to_string(),
            schema: self.schema.clone(),
        };
        spool_if_changed(&self.name, &batch, store, spool, &mut stats)?;
        stats.files_drained = 1;
        Ok(stats)
    }
}

/// Render a KPI file into stamped NDJSON records.
fn render_kpi(raw: &[u8], format: KpiFormat, schema: &str, captured_at: &str) -> (Vec<u8>, u64) {
    let mut ndjson = Vec::with_capacity(raw.len() + 128);
    let mut rows = 0u64;
    match format {
        KpiFormat::Json => {
            push_stamped(&mut ndjson, raw, schema, captured_at);
            rows = 1;
        }
        KpiFormat::Jsonl => {
            for line in raw.split(|&b| b == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() {
                    continue;
                }
                push_stamped(&mut ndjson, line, schema, captured_at);
                rows += 1;
            }
        }
    }
    (ndjson, rows)
}

/// Append one record: a parseable JSON object gets `schema` + `captured_at`
/// stamped in; anything else is wrapped in a stamped envelope so nothing is
/// ever dropped.
fn push_stamped(out: &mut Vec<u8>, raw: &[u8], schema: &str, captured_at: &str) {
    let value = match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(mut map)) => {
            map.insert("schema".to_string(), Value::String(schema.to_string()));
            map.insert(
                "captured_at".to_string(),
                Value::String(captured_at.to_string()),
            );
            Value::Object(map)
        }
        Ok(other) => stamped_envelope(schema, captured_at, other),
        Err(_) => stamped_envelope(
            schema,
            captured_at,
            Value::String(String::from_utf8_lossy(raw).into_owned()),
        ),
    };
    out.extend_from_slice(&serde_json::to_vec(&value).unwrap_or_else(|_| raw.to_vec()));
    out.push(b'\n');
}

fn stamped_envelope(schema: &str, captured_at: &str, payload: Value) -> Value {
    let mut map = Map::new();
    map.insert("schema".to_string(), Value::String(schema.to_string()));
    map.insert(
        "captured_at".to_string(),
        Value::String(captured_at.to_string()),
    );
    map.insert("payload".to_string(), payload);
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// ldesk legatus.db resolution + read-only access
// ---------------------------------------------------------------------------

/// Resolve the ldesk `legatus.db` path. Precedence mirrors ldesk's own
/// `paths::data_dir()` so the two sides can never disagree:
///
/// 1. `SENTINEL_LDESK_DB` — explicit db-file override (tests, exotic setups);
/// 2. `LEGATUS_DATA_DIR` — explicit data-dir override (ldesk e2e harness);
/// 3. `LEGATUS_DEV=1` — the dev data dir `io.legatus.desktop.dev/`;
/// 4. default — the production data dir `io.legatus.desktop/`.
#[must_use]
pub fn ldesk_db_path() -> PathBuf {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(db) = var("SENTINEL_LDESK_DB") {
        return PathBuf::from(db);
    }
    if let Some(dir) = var("LEGATUS_DATA_DIR") {
        return PathBuf::from(dir).join("legatus.db");
    }
    let app_dir = if var("LEGATUS_DEV").as_deref() == Some("1") {
        "io.legatus.desktop.dev"
    } else {
        "io.legatus.desktop"
    };
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(app_dir)
        .join("legatus.db")
}

/// Open `legatus.db` strictly read-only (WAL-safe; works whether or not the
/// app is running, and can never mutate the store).
fn open_ro(db_path: &Path) -> Result<rusqlite::Connection> {
    rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open read-only {}", db_path.display()))
}

/// `true` when the db has an `agent_runs` table (a fresh ldesk store may
/// not — that's a skip, not an error).
fn has_agent_runs(conn: &rusqlite::Connection) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'agent_runs'",
            [],
            |r| r.get(0),
        )
        .context("probe agent_runs table")?;
    Ok(n > 0)
}

/// Shared "absent db / absent table" preamble for the sqlite sources.
/// Returns `None` (with a note) when there is nothing to collect.
fn open_agent_runs(
    db_path: &Path,
    stats: &mut SourceStats,
) -> Result<Option<rusqlite::Connection>> {
    if !db_path.exists() {
        stats
            .notes
            .push(format!("ldesk db absent: {}", db_path.display()));
        return Ok(None);
    }
    let conn = open_ro(db_path)?;
    if !has_agent_runs(&conn)? {
        stats.notes.push(format!(
            "ldesk db has no agent_runs table: {}",
            db_path.display()
        ));
        return Ok(None);
    }
    Ok(Some(conn))
}

/// Clamp a nullable SQL sum to `u64` (mirrors ldesk's `.unwrap_or(0).max(0)`).
fn clamp_u64(v: Option<i64>) -> u64 {
    v.unwrap_or(0).max(0) as u64
}

// ---------------------------------------------------------------------------
// (b) agent $/issue usage rollup — the ticket-cost association stream
// ---------------------------------------------------------------------------

/// The agent $/issue rollup snapshot (`v1/usage-by-issue/…`), read from
/// ldesk's `agent_runs` with the exact dedup/bucketing contract of
/// `usage_rollup_by_issue()` in
/// `legatus-infrastructure/src/agents/sqlite_run.rs`.
#[derive(Debug, Clone)]
pub struct UsageRollupSource {
    pub db_path: PathBuf,
}

impl UsageRollupSource {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

/// The rollup query, verbatim from ldesk (resume dedup via
/// `ROW_NUMBER() OVER (PARTITION BY linear_issue_id,
/// COALESCE(claude_session_id, id) ORDER BY started_at DESC)`; NULL issues
/// group into one bucket; RFC3339 `+00:00` text timestamps order
/// lexicographically = chronologically).
const ROLLUP_SQL: &str = "\
    SELECT linear_issue_id,
           COUNT(*) AS runs,
           SUM(usage_input_tokens) AS input_tokens,
           SUM(usage_output_tokens) AS output_tokens,
           SUM(usage_cache_creation_tokens) AS cache_creation_tokens,
           SUM(usage_cache_read_tokens) AS cache_read_tokens,
           SUM(usage_cost_micro_usd) AS cost_micro_usd,
           MAX(activity_at) AS last_activity_at
    FROM (
        SELECT linear_issue_id, usage_input_tokens, usage_output_tokens,
               usage_cache_creation_tokens, usage_cache_read_tokens,
               usage_cost_micro_usd,
               COALESCE(last_activity_at, usage_updated_at, started_at)
                   AS activity_at,
               ROW_NUMBER() OVER (
                   PARTITION BY linear_issue_id, COALESCE(claude_session_id, id)
                   ORDER BY started_at DESC
               ) AS rn
        FROM agent_runs
        WHERE usage_updated_at IS NOT NULL
    )
    WHERE rn = 1
    GROUP BY linear_issue_id
    ORDER BY MAX(activity_at) DESC";

impl TelemetrySource for UsageRollupSource {
    fn name(&self) -> &'static str {
        "usage-by-issue"
    }

    fn collect(&self, store: &mut CheckpointStore, spool: &SpoolConfig) -> Result<SourceStats> {
        let mut stats = SourceStats::default();
        let Some(conn) = open_agent_runs(&self.db_path, &mut stats)? else {
            return Ok(stats);
        };

        let mut stmt = conn.prepare(ROLLUP_SQL).context("prepare rollup query")?;
        let mut ndjson: Vec<u8> = Vec::new();
        let mut rows = 0u64;
        let mut max_activity: Option<String> = None;
        let mut query = stmt.query([]).context("run rollup query")?;
        while let Some(row) = query.next().context("read rollup row")? {
            let issue: Option<String> = row.get(0)?;
            let runs: i64 = row.get(1)?;
            let last_activity: Option<String> = row.get(7)?;
            if let Some(ts) = &last_activity {
                if max_activity.as_deref().is_none_or(|m| ts.as_str() > m) {
                    max_activity = Some(ts.clone());
                }
            }
            let record = serde_json::json!({
                "schema": USAGE_BY_ISSUE_SCHEMA,
                "linear_issue_id": issue.unwrap_or_else(|| UNASSIGNED_BUCKET.to_string()),
                "runs": runs.max(0) as u64,
                "input_tokens": clamp_u64(row.get(2)?),
                "output_tokens": clamp_u64(row.get(3)?),
                "cache_creation_tokens": clamp_u64(row.get(4)?),
                "cache_read_tokens": clamp_u64(row.get(5)?),
                "cost_micro_usd": clamp_u64(row.get(6)?),
                "last_activity_at": last_activity,
            });
            ndjson.extend_from_slice(&serde_json::to_vec(&record).context("encode rollup row")?);
            ndjson.push(b'\n');
            rows += 1;
        }
        if rows == 0 {
            stats.notes.push("no usage rows in agent_runs".to_string());
            return Ok(stats);
        }

        // Deterministic capture stamp: the newest activity in the data.
        let captured_at = max_activity.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let ndjson = stamp_captured_at(&ndjson, &captured_at)?;
        let batch = SnapshotBatch {
            gate_sha256: sha256_hex(&ndjson),
            rows,
            captured_at,
            key_base: "v1/usage-by-issue".to_string(),
            kind: "usage".to_string(),
            schema: USAGE_BY_ISSUE_SCHEMA.to_string(),
            ndjson,
        };
        spool_if_changed("usage-by-issue", &batch, store, spool, &mut stats)?;
        stats.files_drained = 1;
        Ok(stats)
    }
}

// ---------------------------------------------------------------------------
// session → issue mapping — joins hook-ledger session_id to tickets
// ---------------------------------------------------------------------------

/// The minimal `claude_session_id → linear_issue_id` map
/// (`v1/session-issue/…`) so lake queries can attribute hook *activity*
/// (ledger rows carry `session_id`) to a ticket, not just cost.
#[derive(Debug, Clone)]
pub struct SessionIssueSource {
    pub db_path: PathBuf,
}

impl SessionIssueSource {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

/// Distinct session→issue pairs with a deterministic order and a
/// `last_seen_at` per pair (max of the runs' activity stamps).
const SESSION_ISSUE_SQL: &str = "\
    SELECT claude_session_id, linear_issue_id,
           MAX(COALESCE(last_activity_at, usage_updated_at, started_at)) AS last_seen_at
    FROM agent_runs
    WHERE claude_session_id IS NOT NULL
    GROUP BY claude_session_id, linear_issue_id
    ORDER BY claude_session_id, linear_issue_id";

impl TelemetrySource for SessionIssueSource {
    fn name(&self) -> &'static str {
        "session-issue"
    }

    fn collect(&self, store: &mut CheckpointStore, spool: &SpoolConfig) -> Result<SourceStats> {
        let mut stats = SourceStats::default();
        let Some(conn) = open_agent_runs(&self.db_path, &mut stats)? else {
            return Ok(stats);
        };

        let mut stmt = conn
            .prepare(SESSION_ISSUE_SQL)
            .context("prepare session-issue query")?;
        let mut ndjson: Vec<u8> = Vec::new();
        let mut rows = 0u64;
        let mut max_seen: Option<String> = None;
        let mut query = stmt.query([]).context("run session-issue query")?;
        while let Some(row) = query.next().context("read session-issue row")? {
            let session: String = row.get(0)?;
            let issue: Option<String> = row.get(1)?;
            let last_seen: Option<String> = row.get(2)?;
            if let Some(ts) = &last_seen {
                if max_seen.as_deref().is_none_or(|m| ts.as_str() > m) {
                    max_seen = Some(ts.clone());
                }
            }
            let record = serde_json::json!({
                "schema": SESSION_ISSUE_SCHEMA,
                "claude_session_id": session,
                // null = the run was never tied to a ticket ("unassigned").
                "linear_issue_id": issue,
                "last_seen_at": last_seen,
            });
            ndjson.extend_from_slice(&serde_json::to_vec(&record).context("encode session row")?);
            ndjson.push(b'\n');
            rows += 1;
        }
        if rows == 0 {
            stats.notes.push("no sessions in agent_runs".to_string());
            return Ok(stats);
        }

        let captured_at = max_seen.unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let ndjson = stamp_captured_at(&ndjson, &captured_at)?;
        let batch = SnapshotBatch {
            gate_sha256: sha256_hex(&ndjson),
            rows,
            captured_at,
            key_base: "v1/session-issue".to_string(),
            kind: "usage".to_string(),
            schema: SESSION_ISSUE_SCHEMA.to_string(),
            ndjson,
        };
        spool_if_changed("session-issue", &batch, store, spool, &mut stats)?;
        stats.files_drained = 1;
        Ok(stats)
    }
}

/// Stamp `captured_at` into every record of an NDJSON buffer. Done as a
/// second pass because the stamp (max activity) is only known after the
/// full scan; output stays deterministic for a given db state.
fn stamp_captured_at(ndjson: &[u8], captured_at: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(ndjson.len() + 64);
    for line in ndjson.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let mut value: Value = serde_json::from_slice(line).context("re-parse snapshot row")?;
        if let Value::Object(map) = &mut value {
            map.insert(
                "captured_at".to_string(),
                Value::String(captured_at.to_string()),
            );
        }
        out.extend_from_slice(&serde_json::to_vec(&value).context("encode snapshot row")?);
        out.push(b'\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::ledger::collect_sources;
    use crate::telemetry::spool::list_manifests;

    struct Harness {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        checkpoint: PathBuf,
        spool: SpoolConfig,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let checkpoint = root.join("telemetry").join("checkpoint.json");
            let spool = SpoolConfig::new(root.join("telemetry").join("spool"));
            Self {
                _tmp: tmp,
                root,
                checkpoint,
                spool,
            }
        }

        fn collect(&self, source: &dyn TelemetrySource) -> SourceStats {
            let sources: Vec<&dyn TelemetrySource> = vec![source];
            collect_sources(&sources, &self.checkpoint, &self.spool)
                .unwrap()
                .remove(0)
                .1
        }

        /// All spooled rows across all batches, in spool order.
        fn spooled_rows(&self) -> Vec<Value> {
            let mut rows = Vec::new();
            for (_, manifest) in list_manifests(&self.spool.dir).unwrap() {
                let compressed = fs::read(self.spool.dir.join(&manifest.data_file)).unwrap();
                let raw = zstd::decode_all(compressed.as_slice()).unwrap();
                for line in raw.split(|&b| b == b'\n') {
                    if !line.is_empty() {
                        rows.push(serde_json::from_slice(line).unwrap());
                    }
                }
            }
            rows
        }

        fn manifest_keys(&self) -> Vec<String> {
            list_manifests(&self.spool.dir)
                .unwrap()
                .into_iter()
                .map(|(_, m)| m.object_key)
                .collect()
        }
    }

    // -- KPI file snapshots --------------------------------------------------

    #[test]
    fn kpi_unchanged_summary_spools_nothing_changed_spools_one() {
        let h = Harness::new();
        let path = h.root.join("roi-summary.json");
        fs::write(&path, br#"{"roi_ratio": 12.5, "tickets_shipped_total": 3}"#).unwrap();
        let src = KpiFileSource::new("roi", "roi-summary.v1", path.clone(), KpiFormat::Json);

        let stats = h.collect(&src);
        assert_eq!((stats.rows, stats.batches), (1, 1));

        // Unchanged content → zero objects, even across runs.
        let stats = h.collect(&src);
        assert_eq!((stats.rows, stats.batches), (0, 0));
        assert_eq!(h.manifest_keys().len(), 1);

        // mtime-only touch with identical bytes → still zero objects.
        let raw = fs::read(&path).unwrap();
        fs::write(&path, &raw).unwrap();
        let stats = h.collect(&src);
        assert_eq!((stats.rows, stats.batches), (0, 0));

        // Changed content → exactly one new object.
        fs::write(&path, br#"{"roi_ratio": 99.9, "tickets_shipped_total": 4}"#).unwrap();
        let stats = h.collect(&src);
        assert_eq!((stats.rows, stats.batches), (1, 1));
        assert_eq!(h.manifest_keys().len(), 2);
    }

    #[test]
    fn kpi_rows_are_stamped_and_keyed_per_plan() {
        let h = Harness::new();
        let path = h.root.join("roi-summary.json");
        fs::write(&path, br#"{"roi_ratio": 1.0}"#).unwrap();
        let src = KpiFileSource::new("roi", "roi-summary.v1", path, KpiFormat::Json);
        h.collect(&src);

        let rows = h.spooled_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["schema"], "roi-summary.v1");
        assert!(rows[0]["captured_at"].as_str().unwrap().contains('T'));
        assert_eq!(rows[0]["roi_ratio"], 1.0);

        let keys = h.manifest_keys();
        // v1/kpi/{scan}/dt={date}/{captured}_{sha12}.ndjson.zst
        assert!(
            keys[0].starts_with("v1/kpi/roi/dt="),
            "unexpected key {}",
            keys[0]
        );
        assert!(keys[0].ends_with(".ndjson.zst"));
    }

    #[test]
    fn kpi_jsonl_ships_one_stamped_record_per_line() {
        let h = Harness::new();
        let path = h.root.join("tokens-per-ticket.jsonl");
        fs::write(
            &path,
            b"{\"ticket\":\"LEG-1\",\"cost_usd\":1.5}\n{\"ticket\":\"LEG-2\",\"cost_usd\":2.5}\n",
        )
        .unwrap();
        let src = KpiFileSource::new(
            "tokens-per-ticket",
            "tokens-per-ticket.v1",
            path,
            KpiFormat::Jsonl,
        );
        let stats = h.collect(&src);
        assert_eq!(stats.rows, 2);

        let rows = h.spooled_rows();
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(r["schema"], "tokens-per-ticket.v1");
            assert!(r["captured_at"].is_string());
        }
        assert_eq!(rows[0]["ticket"], "LEG-1");
        assert_eq!(rows[1]["ticket"], "LEG-2");
    }

    #[test]
    fn kpi_absent_or_empty_file_is_a_note_not_an_error() {
        let h = Harness::new();
        let path = h.root.join("missing.json");
        let src = KpiFileSource::new("roi", "roi-summary.v1", path.clone(), KpiFormat::Json);
        let stats = h.collect(&src);
        assert_eq!(stats.batches, 0);
        assert!(stats.notes.iter().any(|n| n.contains("absent")));

        fs::write(&path, b"  \n").unwrap();
        let stats = h.collect(&src);
        assert_eq!(stats.batches, 0);
        assert!(stats.notes.iter().any(|n| n.contains("empty")));
    }

    // -- sqlite rollup + session map ------------------------------------------

    /// Minimal `agent_runs` table covering every column the queries touch.
    fn seed_db(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_runs (
                id TEXT PRIMARY KEY,
                linear_issue_id TEXT,
                claude_session_id TEXT,
                started_at TEXT NOT NULL,
                last_activity_at TEXT,
                usage_input_tokens INTEGER,
                usage_output_tokens INTEGER,
                usage_cache_creation_tokens INTEGER,
                usage_cache_read_tokens INTEGER,
                usage_cost_micro_usd INTEGER,
                usage_updated_at TEXT
            );",
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_run(
        conn: &rusqlite::Connection,
        id: &str,
        issue: Option<&str>,
        session: Option<&str>,
        started_at: &str,
        cost: i64,
        input: i64,
        usage_updated: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO agent_runs (id, linear_issue_id, claude_session_id, started_at,
                last_activity_at, usage_input_tokens, usage_output_tokens,
                usage_cache_creation_tokens, usage_cache_read_tokens,
                usage_cost_micro_usd, usage_updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5, 10, 20, 30, ?6, ?7)",
            rusqlite::params![id, issue, session, started_at, input, cost, usage_updated],
        )
        .unwrap();
    }

    /// Seeds the ldesk rollup contract fixture: an assigned issue with two
    /// sessions, a resumed-dup session (two samples, only the newest
    /// counts), an unassigned run, and a no-usage run (excluded).
    fn seed_contract_fixture(db: &Path) {
        let conn = seed_db(db);
        // LEG-100: two distinct sessions → both counted, costs summed.
        insert_run(
            &conn,
            "r1",
            Some("LEG-100"),
            Some("s1"),
            "2026-06-10T10:00:00+00:00",
            100,
            1000,
            Some("2026-06-10T10:05:00+00:00"),
        );
        insert_run(
            &conn,
            "r2",
            Some("LEG-100"),
            Some("s2"),
            "2026-06-10T11:00:00+00:00",
            50,
            500,
            Some("2026-06-10T11:05:00+00:00"),
        );
        // LEG-200: one session resumed → two rows share s3; ONLY the most
        // recent sample (cost 400) may count — summing would double-count
        // the session-cumulative history.
        insert_run(
            &conn,
            "r3",
            Some("LEG-200"),
            Some("s3"),
            "2026-06-11T09:00:00+00:00",
            300,
            3000,
            Some("2026-06-11T09:05:00+00:00"),
        );
        insert_run(
            &conn,
            "r4",
            Some("LEG-200"),
            Some("s3"),
            "2026-06-11T10:00:00+00:00",
            400,
            4000,
            Some("2026-06-11T10:05:00+00:00"),
        );
        // Unassigned run with usage → the "unassigned" bucket.
        insert_run(
            &conn,
            "r5",
            None,
            Some("s4"),
            "2026-06-11T12:00:00+00:00",
            70,
            700,
            Some("2026-06-11T12:05:00+00:00"),
        );
        // Run with no usage sample → excluded from the rollup entirely.
        insert_run(
            &conn,
            "r6",
            Some("LEG-300"),
            Some("s5"),
            "2026-06-11T13:00:00+00:00",
            0,
            0,
            None,
        );
    }

    #[test]
    fn rollup_matches_ldesk_contract_dedup_unassigned_and_sums() {
        let h = Harness::new();
        let db = h.root.join("legatus.db");
        seed_contract_fixture(&db);

        let src = UsageRollupSource::new(db);
        let stats = h.collect(&src);
        assert_eq!(stats.rows, 3, "LEG-100 + LEG-200 + unassigned");

        let rows = h.spooled_rows();
        let by_issue = |k: &str| {
            rows.iter()
                .find(|r| r["linear_issue_id"] == k)
                .unwrap_or_else(|| panic!("missing bucket {k}"))
                .clone()
        };

        let leg100 = by_issue("LEG-100");
        assert_eq!(leg100["runs"], 2);
        assert_eq!(leg100["cost_micro_usd"], 150);
        assert_eq!(leg100["input_tokens"], 1500);

        // Resume dedup: only the newest s3 sample counts.
        let leg200 = by_issue("LEG-200");
        assert_eq!(leg200["runs"], 1);
        assert_eq!(leg200["cost_micro_usd"], 400);
        assert_eq!(leg200["input_tokens"], 4000);

        let unassigned = by_issue(UNASSIGNED_BUCKET);
        assert_eq!(unassigned["runs"], 1);
        assert_eq!(unassigned["cost_micro_usd"], 70);

        // LEG-300 had no usage sample → not in the rollup.
        assert!(!rows.iter().any(|r| r["linear_issue_id"] == "LEG-300"));

        for r in &rows {
            assert_eq!(r["schema"], USAGE_BY_ISSUE_SCHEMA);
            assert_eq!(
                r["captured_at"], "2026-06-11T12:00:00+00:00",
                "captured_at = max activity in the data (deterministic)"
            );
        }

        let keys = h.manifest_keys();
        assert!(
            keys[0].starts_with("v1/usage-by-issue/dt=2026-06-11/"),
            "unexpected key {}",
            keys[0]
        );
    }

    #[test]
    fn rollup_unchanged_db_spools_nothing_new_run_spools_one() {
        let h = Harness::new();
        let db = h.root.join("legatus.db");
        seed_contract_fixture(&db);
        let src = UsageRollupSource::new(db.clone());

        assert_eq!(h.collect(&src).batches, 1);
        // Same db state → zero objects.
        assert_eq!(h.collect(&src).batches, 0);
        assert_eq!(h.manifest_keys().len(), 1);

        // A new run lands → exactly one new snapshot.
        let conn = rusqlite::Connection::open(&db).unwrap();
        insert_run(
            &conn,
            "r7",
            Some("LEG-100"),
            Some("s6"),
            "2026-06-12T08:00:00+00:00",
            25,
            250,
            Some("2026-06-12T08:05:00+00:00"),
        );
        drop(conn);
        let stats = h.collect(&src);
        assert_eq!(stats.batches, 1);
        assert_eq!(h.manifest_keys().len(), 2);
    }

    #[test]
    fn session_issue_map_covers_assigned_unassigned_and_dedups_pairs() {
        let h = Harness::new();
        let db = h.root.join("legatus.db");
        seed_contract_fixture(&db);

        let src = SessionIssueSource::new(db);
        let stats = h.collect(&src);
        // s1→LEG-100, s2→LEG-100, s3→LEG-200 (two rows, ONE pair),
        // s4→null, s5→LEG-300 (no usage needed for the map).
        assert_eq!(stats.rows, 5);

        let rows = h.spooled_rows();
        let pair = |s: &str| {
            rows.iter()
                .find(|r| r["claude_session_id"] == s)
                .unwrap_or_else(|| panic!("missing session {s}"))
                .clone()
        };
        assert_eq!(pair("s1")["linear_issue_id"], "LEG-100");
        assert_eq!(pair("s3")["linear_issue_id"], "LEG-200");
        assert!(pair("s4")["linear_issue_id"].is_null());
        assert_eq!(pair("s5")["linear_issue_id"], "LEG-300");
        assert_eq!(
            rows.iter()
                .filter(|r| r["claude_session_id"] == "s3")
                .count(),
            1,
            "resumed session collapses to one mapping row"
        );
        for r in &rows {
            assert_eq!(r["schema"], SESSION_ISSUE_SCHEMA);
            assert!(r["captured_at"].is_string());
        }

        let keys = h.manifest_keys();
        assert!(
            keys[0].starts_with("v1/session-issue/dt="),
            "unexpected key {}",
            keys[0]
        );
    }

    #[test]
    fn sqlite_sources_tolerate_missing_db_and_missing_table() {
        let h = Harness::new();
        let missing = h.root.join("nope.db");
        let stats = h.collect(&UsageRollupSource::new(missing));
        assert_eq!(stats.batches, 0);
        assert!(stats.notes.iter().any(|n| n.contains("absent")));

        // A db without agent_runs (fresh store) is a note, not an error.
        let empty = h.root.join("empty.db");
        rusqlite::Connection::open(&empty)
            .unwrap()
            .execute_batch("CREATE TABLE other (id TEXT);")
            .unwrap();
        let stats = h.collect(&SessionIssueSource::new(empty));
        assert_eq!(stats.batches, 0);
        assert!(stats.notes.iter().any(|n| n.contains("no agent_runs")));
    }

    #[test]
    fn db_is_opened_read_only() {
        let h = Harness::new();
        let db = h.root.join("legatus.db");
        seed_db(&db);
        let conn = open_ro(&db).unwrap();
        let err = conn
            .execute(
                "INSERT INTO agent_runs (id, started_at) VALUES ('x', 'now')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("readonly"),
            "write must fail on the ro connection: {err}"
        );
    }

    #[test]
    fn crash_window_rerun_reproduces_identical_snapshot_batch() {
        let h = Harness::new();
        let db = h.root.join("legatus.db");
        seed_contract_fixture(&db);
        let src = UsageRollupSource::new(db);
        h.collect(&src);
        let keys_before = h.manifest_keys();

        // Crash window: spool write landed, checkpoint persist did not.
        fs::remove_file(&h.checkpoint).unwrap();
        h.collect(&src);
        assert_eq!(
            h.manifest_keys(),
            keys_before,
            "deterministic stamp + content → identical key → overwrite, no dupes"
        );
    }
}
