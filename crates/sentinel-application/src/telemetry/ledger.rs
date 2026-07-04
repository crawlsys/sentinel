//! Append-only ledger tailing + the collect engine (LEG-259).
//!
//! Each [`LedgerSource`] tails one per-harness hook-invocation ledger
//! (`hook-invocations.jsonl`) by byte offset, with the cursor keyed by
//! `(dev, inode)` so sentinel's size-based rotation (rename to
//! `<name>.archive.<ts_ms>`, fresh live file) is detected: the archived file
//! is drained to completion first, then any newer archives, then the live
//! file from offset 0.
//!
//! Reliability contract (the LEG-259 acceptance):
//! - the checkpoint advances only after the spool batch is durably written
//!   (fsync) — a crash in between re-produces the *identical* batch (same
//!   offsets → same content → same content-hash file name → overwrite), so
//!   re-runs never double-spool and never drop;
//! - partial trailing lines (a row mid-append) are never consumed — the
//!   offset stops at the last complete `\n`;
//! - a full spool stalls collection loudly (error propagates, checkpoint
//!   stays put) — never a silent drop.
//!
//! Every spooled row carries `"schema": "ledger.v1"`. Rows that fail to
//! parse as JSON objects are passed through verbatim (lossless) rather than
//! dropped.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use super::checkpoint::{CheckpointStore, FileIdentity, LedgerCursor};
use super::spool::{self, BatchSpec, SpoolConfig, MAX_BATCH_BYTES};

/// Schema tag stamped onto every spooled ledger row.
pub const LEDGER_SCHEMA: &str = "ledger.v1";

/// A collectable telemetry source. [`LedgerSource`] is the only implementor
/// today; the LEG-261 snapshot adapters (KPI summaries, usage rollups) plug
/// into the same seam — `collect` reads its cursor from the store, stages
/// batches into the spool, and persists the cursor after each durable write.
pub trait TelemetrySource {
    fn name(&self) -> &str;

    /// Collect new data into the spool, advancing this source's checkpoint
    /// cursor after every durable batch write.
    fn collect(&self, store: &mut CheckpointStore, spool: &SpoolConfig) -> Result<SourceStats>;
}

/// Per-source outcome of one collect run.
#[derive(Debug, Default, Clone)]
pub struct SourceStats {
    pub rows: u64,
    pub batches: u64,
    pub spooled_bytes: u64,
    pub files_drained: u32,
    /// Loud, human-readable warnings (lost checkpoint identity, offset past
    /// EOF, …) surfaced by the CLI.
    pub notes: Vec<String>,
}

/// One append-only JSONL ledger (live file + rotated `.archive.<ts_ms>`
/// siblings).
#[derive(Debug, Clone)]
pub struct LedgerSource {
    pub name: String,
    pub live_path: PathBuf,
}

impl LedgerSource {
    #[must_use]
    pub const fn new(name: String, live_path: PathBuf) -> Self {
        Self { name, live_path }
    }
}

/// The per-harness ledgers the KPI report aggregates (mirrors ldesk's
/// `sentinel_ledger_paths()`): Claude under the sentinel claude dir (honors
/// `SENTINEL_CLAUDE_DIR`), the Codex shim under `~/.codex`, and the isolated
/// `OpenCode` agent-home.
#[must_use]
pub fn default_ledger_sources() -> Vec<LedgerSource> {
    const LEDGER_REL: &str = "sentinel/metrics/hook-invocations.jsonl";
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    vec![
        LedgerSource::new(
            "claude".to_string(),
            crate::paths::claude_dir().join(LEDGER_REL),
        ),
        LedgerSource::new("codex".to_string(), home.join(".codex").join(LEDGER_REL)),
        LedgerSource::new(
            "opencode".to_string(),
            home.join(".config/legatus/agent-homes/opencode/.claude")
                .join(LEDGER_REL),
        ),
    ]
}

/// Run every source against one shared checkpoint file. Errors (including
/// the spool-cap stall) propagate — a one-shot run exits non-zero and the
/// next timer fire resumes from the persisted checkpoint.
pub fn collect_sources(
    sources: &[&dyn TelemetrySource],
    checkpoint_path: &Path,
    spool: &SpoolConfig,
) -> Result<Vec<(String, SourceStats)>> {
    let mut store = CheckpointStore::load(checkpoint_path)?;
    let mut out = Vec::with_capacity(sources.len());
    for source in sources {
        let stats = source
            .collect(&mut store, spool)
            .with_context(|| format!("collect source `{}`", source.name()))?;
        out.push((source.name().to_string(), stats));
    }
    Ok(out)
}

impl TelemetrySource for LedgerSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn collect(&self, store: &mut CheckpointStore, spool: &SpoolConfig) -> Result<SourceStats> {
        let mut stats = SourceStats::default();
        let archives = list_archives(&self.live_path)?;
        let live_exists = self.live_path.exists();
        if !live_exists && archives.is_empty() {
            stats
                .notes
                .push(format!("ledger absent: {}", self.live_path.display()));
            return Ok(stats);
        }

        let plan = self.drain_plan(store.ledger(&self.name), &archives, live_exists, &mut stats)?;
        for (path, start_offset) in &plan {
            self.drain_file(path, *start_offset, store, spool, &mut stats)?;
        }
        Ok(stats)
    }
}

impl LedgerSource {
    /// Decide which files to drain, in order, and from what starting offset.
    fn drain_plan(
        &self,
        cursor: Option<LedgerCursor>,
        archives: &[PathBuf],
        live_exists: bool,
        stats: &mut SourceStats,
    ) -> Result<Vec<(PathBuf, u64)>> {
        let mut plan: Vec<(PathBuf, u64)> = Vec::new();
        let push_all_from_scratch = |plan: &mut Vec<(PathBuf, u64)>| {
            for a in archives {
                plan.push((a.clone(), 0));
            }
            if live_exists {
                plan.push((self.live_path.clone(), 0));
            }
        };

        let Some(cursor) = cursor else {
            // First run: everything from the beginning, oldest archive first.
            push_all_from_scratch(&mut plan);
            return Ok(plan);
        };

        // Locate the checkpointed file by identity among archives + live.
        let mut found_at: Option<usize> = None; // index into archives
        for (i, a) in archives.iter().enumerate() {
            let meta = fs::metadata(a).with_context(|| format!("stat {}", a.display()))?;
            if FileIdentity::of(&meta) == cursor.identity() {
                found_at = Some(i);
                break;
            }
        }

        if let Some(i) = found_at {
            // Rotation happened: finish the archived file from our offset,
            // then any newer archives, then the fresh live file.
            plan.push((archives[i].clone(), cursor.offset));
            for a in &archives[i + 1..] {
                plan.push((a.clone(), 0));
            }
            if live_exists {
                plan.push((self.live_path.clone(), 0));
            }
            return Ok(plan);
        }

        if live_exists {
            let meta = fs::metadata(&self.live_path)
                .with_context(|| format!("stat {}", self.live_path.display()))?;
            if FileIdentity::of(&meta) == cursor.identity() {
                // No rotation since last run: resume the live file.
                let start = if cursor.offset <= meta.len() {
                    cursor.offset
                } else {
                    stats.notes.push(format!(
                        "checkpoint offset {} past EOF of {} — file shrank? restarting from 0 \
                         (may re-spool)",
                        cursor.offset,
                        self.live_path.display(),
                    ));
                    0
                };
                plan.push((self.live_path.clone(), start));
                return Ok(plan);
            }
        }

        // Checkpointed file no longer exists anywhere (archives pruned?).
        // Never drop: collect everything from scratch. Content-hash batch
        // ids dedupe exact re-spools; genuinely-new boundaries may duplicate
        // — at-least-once beats data loss, and we say so loudly.
        stats.notes.push(format!(
            "checkpoint identity (dev={}, ino={}) not found for `{}` — archives pruned? \
             collecting from scratch (at-least-once: duplicates possible)",
            cursor.dev, cursor.ino, self.name,
        ));
        push_all_from_scratch(&mut plan);
        Ok(plan)
    }

    /// Drain one file from `start_offset` to its last complete line,
    /// spooling ≤4 MB batches and persisting the checkpoint after each.
    fn drain_file(
        &self,
        path: &Path,
        start_offset: u64,
        store: &mut CheckpointStore,
        spool: &SpoolConfig,
        stats: &mut SourceStats,
    ) -> Result<()> {
        let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let id = FileIdentity::of(&meta);

        let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        f.seek(SeekFrom::Start(start_offset))
            .with_context(|| format!("seek {}", path.display()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;

        // Only complete lines are consumable; a partial trailing line stays
        // for the next run.
        let complete = buf.iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);

        let mut consumed: usize = 0;
        while consumed < complete {
            let (batch, len) = build_batch(&buf[consumed..complete]);
            if batch.rows > 0 {
                let spec = self.batch_spec(&batch);
                let manifest = spool::write_batch(spool, &spec, &batch.ndjson)?;
                stats.rows += batch.rows;
                stats.batches += 1;
                stats.spooled_bytes += manifest.compressed_bytes;
            }
            consumed += len;
            store.set_ledger(
                &self.name,
                LedgerCursor {
                    dev: id.dev,
                    ino: id.ino,
                    offset: start_offset + consumed as u64,
                },
            );
            store.persist()?;
        }

        // Even when the file contributed nothing (e.g. a fresh live file
        // after rotation), move the cursor onto it so the next run resumes
        // here directly.
        let final_cursor = LedgerCursor {
            dev: id.dev,
            ino: id.ino,
            offset: start_offset + consumed as u64,
        };
        if store.ledger(&self.name) != Some(final_cursor) {
            store.set_ledger(&self.name, final_cursor);
            store.persist()?;
        }
        stats.files_drained += 1;
        Ok(())
    }

    fn batch_spec(&self, batch: &TransformedBatch) -> BatchSpec {
        // Deterministic stamps: derived from row timestamps, never from
        // wall-clock capture time, so a crash-window re-run reproduces the
        // identical file name. `unknown` is the (deterministic) fallback for
        // batches whose rows carry no parseable ts.
        let first = batch
            .first_ts
            .as_deref()
            .and_then(compact_ts)
            .unwrap_or_else(|| "unknown".to_string());
        let last = batch
            .last_ts
            .as_deref()
            .and_then(compact_ts)
            .unwrap_or_else(|| "unknown".to_string());
        let dt = batch
            .last_ts
            .as_deref()
            .and_then(date_of_ts)
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d").to_string());
        BatchSpec {
            source: self.name.clone(),
            kind: "ledger".to_string(),
            schema: LEDGER_SCHEMA.to_string(),
            key_prefix: format!("v1/ledger/{}/dt={dt}", self.name),
            stamp: format!("{first}_{last}"),
            rows: batch.rows,
            first_ts: batch.first_ts.clone(),
            last_ts: batch.last_ts.clone(),
        }
    }
}

struct TransformedBatch {
    ndjson: Vec<u8>,
    rows: u64,
    first_ts: Option<String>,
    last_ts: Option<String>,
}

/// Build the next ≤[`MAX_BATCH_BYTES`] batch from `data` (which is
/// line-complete): stamp `"schema": "ledger.v1"` onto every parseable row,
/// pass anything unparseable through verbatim (never drop), and cut the
/// batch at the line boundary where the *transformed* output would exceed
/// the cap. A single oversized line still ships alone (never drop). Returns
/// the batch plus the number of RAW bytes consumed (for the checkpoint
/// offset). Fully deterministic for a given input — crash-window re-runs
/// reproduce byte-identical batches.
fn build_batch(data: &[u8]) -> (TransformedBatch, usize) {
    let mut ndjson = Vec::with_capacity(data.len().min(MAX_BATCH_BYTES) + 64);
    let mut rows: u64 = 0;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut raw_consumed: usize = 0;

    let mut rest = data;
    while let Some(nl) = rest.iter().position(|&b| b == b'\n') {
        let (line_with_nl, tail) = rest.split_at(nl + 1);
        let line = trim_cr(&line_with_nl[..nl]);

        if !line.is_empty() {
            let (bytes, ts) = transform_line(line);
            // Cut BEFORE this line if it would push the batch over the cap
            // (unless the batch is empty — an oversized single line ships
            // alone rather than being dropped).
            if rows > 0 && ndjson.len() + bytes.len() + 1 > MAX_BATCH_BYTES {
                break;
            }
            ndjson.extend_from_slice(&bytes);
            ndjson.push(b'\n');
            rows += 1;
            if let Some(ts) = ts {
                if first_ts.is_none() {
                    first_ts = Some(ts.clone());
                }
                last_ts = Some(ts);
            }
        }
        raw_consumed += line_with_nl.len();
        rest = tail;
    }

    (
        TransformedBatch {
            ndjson,
            rows,
            first_ts,
            last_ts,
        },
        raw_consumed,
    )
}

/// Transform one raw ledger line: parseable JSON objects get the
/// `"schema": "ledger.v1"` stamp (deterministic re-serialization) and
/// report their `ts` field; anything else passes through verbatim.
fn transform_line(line: &[u8]) -> (Vec<u8>, Option<String>) {
    match serde_json::from_slice::<Value>(line) {
        Ok(Value::Object(mut map)) => {
            let ts = map
                .get("ts")
                .and_then(Value::as_str)
                .map(std::string::ToString::to_string);
            map.insert(
                "schema".to_string(),
                Value::String(LEDGER_SCHEMA.to_string()),
            );
            let bytes = serde_json::to_vec(&Value::Object(map)).unwrap_or_else(|_| line.to_vec());
            (bytes, ts)
        }
        _ => (line.to_vec(), None),
    }
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// `2026-06-12T01:02:03.456+00:00` → `20260612T010203Z` (UTC).
pub(crate) fn compact_ts(rfc3339: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        })
}

/// RFC3339 ts → `yyyy-mm-dd` (UTC) for the `dt=` partition.
pub(crate) fn date_of_ts(rfc3339: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d")
                .to_string()
        })
}

/// Rotated siblings of `live`, sorted oldest-first by the `<ts_ms>` suffix
/// sentinel's rotation appends.
fn list_archives(live: &Path) -> Result<Vec<PathBuf>> {
    let Some(parent) = live.parent() else {
        return Ok(Vec::new());
    };
    let Some(file_name) = live.file_name().and_then(|n| n.to_str()) else {
        return Ok(Vec::new());
    };
    let prefix = format!("{file_name}.archive.");
    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(err).with_context(|| format!("read ledger dir {}", parent.display()))
        }
    };
    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.filter_map(std::result::Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(suffix) = name.strip_prefix(&prefix) {
            if let Ok(ts) = suffix.parse::<u64>() {
                found.push((ts, entry.path()));
            }
        }
    }
    found.sort_by_key(|(ts, _)| *ts);
    Ok(found.into_iter().map(|(_, p)| p).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    struct Harness {
        _tmp: tempfile::TempDir,
        ledger: PathBuf,
        checkpoint: PathBuf,
        spool: SpoolConfig,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let ledger = tmp.path().join("metrics").join("hook-invocations.jsonl");
            fs::create_dir_all(ledger.parent().unwrap()).unwrap();
            let checkpoint = tmp.path().join("telemetry").join("checkpoint.json");
            let spool = SpoolConfig::new(tmp.path().join("telemetry").join("spool"));
            Self {
                _tmp: tmp,
                ledger,
                checkpoint,
                spool,
            }
        }

        fn source(&self) -> LedgerSource {
            LedgerSource::new("claude".to_string(), self.ledger.clone())
        }

        fn collect(&self) -> Result<SourceStats> {
            let source = self.source();
            let sources: Vec<&dyn TelemetrySource> = vec![&source];
            let mut all = collect_sources(&sources, &self.checkpoint, &self.spool)?;
            Ok(all.remove(0).1)
        }

        fn append(&self, lines: &[&str]) {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.ledger)
                .unwrap();
            for l in lines {
                writeln!(f, "{l}").unwrap();
            }
        }

        fn append_raw(&self, bytes: &[u8]) {
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.ledger)
                .unwrap();
            f.write_all(bytes).unwrap();
        }

        /// All spooled rows, decompressed, in spool order.
        fn spooled_rows(&self) -> Vec<Value> {
            let mut rows = Vec::new();
            for (_, manifest) in spool::list_manifests(&self.spool.dir).unwrap() {
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

        fn spool_file_names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.spool.dir)
                .map(|rd| {
                    rd.filter_map(std::result::Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    fn row(n: u32) -> String {
        format!(
            "{{\"event\":\"PreToolUse\",\"hook\":\"h{n}\",\"outcome\":\"allow\",\
             \"ts\":\"2026-06-12T01:02:{:02}+00:00\"}}",
            n % 60
        )
    }

    #[test]
    fn collect_then_resume_spools_only_new_rows() {
        let h = Harness::new();
        h.append(&[&row(1), &row(2)]);

        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 2);
        assert_eq!(h.spooled_rows().len(), 2);

        // Idempotent re-run with no new data: nothing new spooled.
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 0);
        assert_eq!(h.spooled_rows().len(), 2);

        // Append two more — only they get spooled.
        h.append(&[&row(3), &row(4)]);
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 2);
        let rows = h.spooled_rows();
        assert_eq!(rows.len(), 4);
        let hooks: Vec<&str> = rows.iter().map(|r| r["hook"].as_str().unwrap()).collect();
        assert_eq!(hooks, vec!["h1", "h2", "h3", "h4"]);
    }

    #[test]
    fn rows_carry_ledger_v1_schema() {
        let h = Harness::new();
        h.append(&[&row(1)]);
        h.collect().unwrap();
        for r in h.spooled_rows() {
            assert_eq!(r["schema"], "ledger.v1");
        }
    }

    #[test]
    fn rotation_archive_drained_before_fresh_live_file() {
        let h = Harness::new();
        h.append(&[&row(1), &row(2)]);
        h.collect().unwrap();

        // Rows 3–4 land, then rotation strikes (rename, like sentinel's
        // rotate_metrics_log_if_oversized), then 5–6 land in the new live.
        h.append(&[&row(3), &row(4)]);
        let archive = h
            .ledger
            .with_file_name("hook-invocations.jsonl.archive.1779992465873");
        fs::rename(&h.ledger, &archive).unwrap();
        h.append(&[&row(5), &row(6)]);

        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 4, "archive remainder + new live, exactly once");
        assert_eq!(stats.files_drained, 2);
        let hooks: Vec<String> = h
            .spooled_rows()
            .iter()
            .map(|r| r["hook"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(hooks, vec!["h1", "h2", "h3", "h4", "h5", "h6"]);

        // Next run resumes on the live file — nothing re-spooled.
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 0);
    }

    #[test]
    fn multiple_archives_drained_oldest_first_on_first_run() {
        let h = Harness::new();
        h.append(&[&row(1)]);
        fs::rename(
            &h.ledger,
            h.ledger
                .with_file_name("hook-invocations.jsonl.archive.100"),
        )
        .unwrap();
        h.append(&[&row(2)]);
        fs::rename(
            &h.ledger,
            h.ledger
                .with_file_name("hook-invocations.jsonl.archive.200"),
        )
        .unwrap();
        h.append(&[&row(3)]);

        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 3);
        assert_eq!(stats.files_drained, 3);
        let hooks: Vec<String> = h
            .spooled_rows()
            .iter()
            .map(|r| r["hook"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(hooks, vec!["h1", "h2", "h3"]);
    }

    #[test]
    fn partial_trailing_line_is_not_consumed_until_complete() {
        let h = Harness::new();
        h.append(&[&row(1)]);
        h.append_raw(b"{\"event\":\"PreToolUse\",\"hook\":\"partial\""); // no \n

        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 1, "partial line must not be consumed");

        // Writer finishes the line; only then is it collected — once.
        h.append_raw(b",\"outcome\":\"allow\",\"ts\":\"2026-06-12T01:03:00+00:00\"}\n");
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 1);
        let rows = h.spooled_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["hook"], "partial");
    }

    #[test]
    fn crash_window_rerun_reproduces_identical_batches_no_double_spool() {
        let h = Harness::new();
        h.append(&[&row(1), &row(2), &row(3)]);
        h.collect().unwrap();
        let names_before = h.spool_file_names();

        // Simulate the crash window: spool write landed but the checkpoint
        // persist did not — i.e. the checkpoint is rolled back to zero.
        fs::remove_file(&h.checkpoint).unwrap();
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 3, "re-run re-reads the same byte range");
        assert_eq!(
            h.spool_file_names(),
            names_before,
            "identical content → identical batch file names → overwrite, no dupes"
        );
        assert_eq!(h.spooled_rows().len(), 3);
    }

    #[test]
    fn spool_cap_stalls_loudly_and_checkpoint_does_not_advance() {
        let h = Harness::new();
        let tiny = SpoolConfig::new(h.spool.dir.clone()).with_cap(4);
        h.append(&[&row(1)]);

        let source = h.source();
        let sources: Vec<&dyn TelemetrySource> = vec![&source];
        let err = collect_sources(&sources, &h.checkpoint, &tiny).unwrap_err();
        assert!(format!("{err:#}").contains("spool cap exceeded"), "{err:#}");

        // Nothing spooled, checkpoint never advanced → a later run with a
        // drained spool picks the row up.
        assert_eq!(h.spooled_rows().len(), 0);
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 1);
    }

    #[test]
    fn unparseable_lines_pass_through_verbatim_never_dropped() {
        let h = Harness::new();
        h.append(&[&row(1), "this is not json", &row(2)]);
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 3);

        let (_, manifest) = spool::list_manifests(&h.spool.dir).unwrap().remove(0);
        let raw = zstd::decode_all(
            fs::read(h.spool.dir.join(&manifest.data_file))
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let lines: Vec<&[u8]> = raw
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], b"this is not json");
    }

    #[test]
    fn large_input_splits_into_multiple_batches_at_line_boundaries() {
        let h = Harness::new();
        // ~1100 rows of ~5KB → >4MB uncompressed → at least 2 batches.
        let filler = "x".repeat(5000);
        let mut lines = Vec::new();
        for n in 0..1100 {
            lines.push(format!(
                "{{\"hook\":\"h{n}\",\"pad\":\"{filler}\",\"ts\":\"2026-06-12T01:02:03+00:00\"}}"
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        h.append(&refs);

        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 1100);
        assert!(
            stats.batches >= 2,
            "expected multiple batches, got {}",
            stats.batches
        );
        assert_eq!(h.spooled_rows().len(), 1100);

        for (_, m) in spool::list_manifests(&h.spool.dir).unwrap() {
            assert!(m.uncompressed_bytes <= MAX_BATCH_BYTES as u64);
            assert_eq!(m.schema, "ledger.v1");
            assert!(m.object_key.starts_with("v1/ledger/claude/dt=2026-06-12/"));
        }
    }

    #[test]
    fn object_key_layout_matches_plan() {
        let h = Harness::new();
        h.append(&[&row(1), &row(2)]);
        h.collect().unwrap();
        let (_, m) = spool::list_manifests(&h.spool.dir).unwrap().remove(0);
        // v1/ledger/{harness}/dt={yyyy-mm-dd}/{first}_{last}_{sha12}.ndjson.zst
        assert_eq!(
            m.object_key,
            format!(
                "v1/ledger/claude/dt=2026-06-12/20260612T010201Z_20260612T010202Z_{}.ndjson.zst",
                &m.sha256[..12]
            )
        );
    }

    #[test]
    fn absent_ledger_is_a_note_not_an_error() {
        let h = Harness::new();
        // No file created at all.
        let stats = h.collect().unwrap();
        assert_eq!(stats.rows, 0);
        assert!(stats.notes.iter().any(|n| n.contains("ledger absent")));
    }
}
