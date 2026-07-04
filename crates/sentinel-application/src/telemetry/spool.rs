//! Durable local spool — zstd-compressed NDJSON batch files plus JSON
//! manifests, staged for upload by `sentinel telemetry ship`.
//!
//! Batch identity is the content: the file name (and the R2 object key in
//! the manifest) embed `sha256[..12]` of the *uncompressed* NDJSON, so
//! re-spooling the same byte range overwrites an identical file instead of
//! duplicating it, and a retried PUT overwrites identical bytes. The hash is
//! taken over the uncompressed content so it stays stable across zstd
//! versions.
//!
//! Write order per batch: data file (tmp + fsync + rename), then manifest
//! (same dance), then best-effort dir fsync. The collector advances its
//! checkpoint only after `write_batch` returns, so a crash anywhere in
//! between re-produces the identical batch on the next run.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Spool size cap — when the spool would exceed this, collection stalls
/// loudly (the checkpoint stops advancing; source files retain the data).
/// Never silently drops.
pub const DEFAULT_SPOOL_CAP_BYTES: u64 = 500 * 1024 * 1024;

/// Maximum uncompressed bytes per batch file.
pub const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Manifest file suffix (the data file it describes sits next to it).
pub const MANIFEST_SUFFIX: &str = ".manifest.json";

/// zstd compression level for batch files.
const ZSTD_LEVEL: i32 = 3;

/// Where the spool lives and how big it may grow.
#[derive(Debug, Clone)]
pub struct SpoolConfig {
    pub dir: PathBuf,
    pub cap_bytes: u64,
}

impl SpoolConfig {
    #[must_use]
    pub const fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cap_bytes: DEFAULT_SPOOL_CAP_BYTES,
        }
    }

    #[must_use]
    pub const fn with_cap(mut self, cap_bytes: u64) -> Self {
        self.cap_bytes = cap_bytes;
        self
    }
}

/// Caller-supplied identity of a batch about to be spooled.
#[derive(Debug, Clone)]
pub struct BatchSpec {
    /// Source name, e.g. `claude` / `codex` / `opencode`.
    pub source: String,
    /// Source kind, e.g. `ledger` (LEG-261 adds `kpi` / `usage`).
    pub kind: String,
    /// Per-row schema tag, e.g. `ledger.v1`.
    pub schema: String,
    /// R2 key directory without trailing slash, e.g.
    /// `v1/ledger/claude/dt=2026-06-12`.
    pub key_prefix: String,
    /// Time-range stamp for the file name, e.g.
    /// `20260612T010203Z_20260612T020304Z`.
    pub stamp: String,
    pub rows: u64,
    /// RFC3339 ts of the first/last parseable row in the batch.
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

/// On-disk manifest describing one spooled batch. `sentinel telemetry ship`
/// reads these to know what to PUT where; it never has to recompute keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchManifest {
    pub manifest_version: u32,
    pub source: String,
    pub kind: String,
    pub schema: String,
    /// Full R2 object key, e.g.
    /// `v1/ledger/claude/dt=2026-06-12/<stamp>_<sha12>.ndjson.zst`.
    pub object_key: String,
    /// File name of the zstd data file sitting next to this manifest.
    pub data_file: String,
    pub rows: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    /// Hex sha256 of the uncompressed NDJSON content.
    pub sha256: String,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
    /// RFC3339 time this batch was spooled (informational; not part of the
    /// batch identity, so crash-window re-spools stay idempotent).
    pub captured_at: String,
}

/// Hex sha256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Total bytes currently in the spool dir (top level only — the spool has
/// no subdirectories).
#[must_use]
pub fn dir_usage_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|e| e.metadata().ok())
        .filter(fs::Metadata::is_file)
        .map(|m| m.len())
        .sum()
}

/// Compress and durably write one batch (data file + manifest) into the
/// spool. Refuses (loudly) when the spool cap would be exceeded — the
/// caller must NOT advance its checkpoint in that case.
pub fn write_batch(cfg: &SpoolConfig, spec: &BatchSpec, ndjson: &[u8]) -> Result<BatchManifest> {
    fs::create_dir_all(&cfg.dir)
        .with_context(|| format!("create spool dir {}", cfg.dir.display()))?;

    let sha256 = sha256_hex(ndjson);
    let sha12 = &sha256[..12];
    let batch_id = format!("{}_{}_{sha12}", spec.source, spec.stamp);
    let data_file = format!("{batch_id}.ndjson.zst");
    let manifest_file = format!("{batch_id}{MANIFEST_SUFFIX}");
    let object_key = format!("{}/{}_{sha12}.ndjson.zst", spec.key_prefix, spec.stamp);

    let compressed = zstd::encode_all(ndjson, ZSTD_LEVEL).context("zstd-compress batch")?;

    let usage = dir_usage_bytes(&cfg.dir);
    let incoming = compressed.len() as u64;
    if usage.saturating_add(incoming) > cfg.cap_bytes {
        anyhow::bail!(
            "telemetry spool cap exceeded ({usage} bytes spooled + {incoming} incoming > \
             {} cap) — STALLING collection at {}. Nothing was dropped: the checkpoint \
             stays put and source files retain the data. Run `sentinel telemetry ship` \
             (or raise the cap) to drain the spool.",
            cfg.cap_bytes,
            cfg.dir.display(),
        );
    }

    let manifest = BatchManifest {
        manifest_version: 1,
        source: spec.source.clone(),
        kind: spec.kind.clone(),
        schema: spec.schema.clone(),
        object_key,
        data_file: data_file.clone(),
        rows: spec.rows,
        uncompressed_bytes: ndjson.len() as u64,
        compressed_bytes: incoming,
        sha256,
        first_ts: spec.first_ts.clone(),
        last_ts: spec.last_ts.clone(),
        captured_at: chrono::Utc::now().to_rfc3339(),
    };

    write_durable(&cfg.dir.join(&data_file), &compressed)?;
    let manifest_json = serde_json::to_vec_pretty(&manifest).context("encode batch manifest")?;
    write_durable(&cfg.dir.join(&manifest_file), &manifest_json)?;

    if let Ok(dir) = fs::File::open(&cfg.dir) {
        let _ = dir.sync_all();
    }
    Ok(manifest)
}

/// All manifests currently in the spool, sorted by file name for a
/// deterministic ship order. Returns `(manifest_path, manifest)` pairs.
pub fn list_manifests(dir: &Path) -> Result<Vec<(PathBuf, BatchManifest)>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err).with_context(|| format!("read spool dir {}", dir.display())),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(MANIFEST_SUFFIX))
        })
        .collect();
    paths.sort();
    for path in paths {
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let manifest: BatchManifest = serde_json::from_str(&raw)
            .with_context(|| format!("parse manifest {}", path.display()))?;
        out.push((path, manifest));
    }
    Ok(out)
}

/// Write `bytes` to `path` atomically: tmp file + fsync + rename.
fn write_durable(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BatchSpec {
        BatchSpec {
            source: "claude".to_string(),
            kind: "ledger".to_string(),
            schema: "ledger.v1".to_string(),
            key_prefix: "v1/ledger/claude/dt=2026-06-12".to_string(),
            stamp: "20260612T010203Z_20260612T020304Z".to_string(),
            rows: 2,
            first_ts: Some("2026-06-12T01:02:03Z".to_string()),
            last_ts: Some("2026-06-12T02:03:04Z".to_string()),
        }
    }

    #[test]
    fn write_batch_round_trips_and_key_embeds_content_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = SpoolConfig::new(tmp.path().to_path_buf());
        let ndjson = b"{\"a\":1}\n{\"a\":2}\n";

        let manifest = write_batch(&cfg, &spec(), ndjson).unwrap();
        let sha = sha256_hex(ndjson);
        assert_eq!(manifest.sha256, sha);
        assert!(manifest
            .object_key
            .starts_with("v1/ledger/claude/dt=2026-06-12/"));
        assert!(manifest.object_key.contains(&sha[..12]));
        assert!(manifest.object_key.ends_with(".ndjson.zst"));

        // Data round-trips through zstd.
        let compressed = fs::read(tmp.path().join(&manifest.data_file)).unwrap();
        let decompressed = zstd::decode_all(compressed.as_slice()).unwrap();
        assert_eq!(decompressed, ndjson);

        // list_manifests sees exactly one batch.
        let listed = list_manifests(tmp.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.object_key, manifest.object_key);
    }

    #[test]
    fn identical_content_overwrites_instead_of_duplicating() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = SpoolConfig::new(tmp.path().to_path_buf());
        let ndjson = b"{\"a\":1}\n";
        write_batch(&cfg, &spec(), ndjson).unwrap();
        write_batch(&cfg, &spec(), ndjson).unwrap();
        // One data file + one manifest, not four files.
        let files: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn cap_exceeded_refuses_loudly_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = SpoolConfig::new(tmp.path().to_path_buf()).with_cap(8);
        let big = vec![b'x'; 4096];
        let mut s = spec();
        s.rows = 1;
        let err = write_batch(&cfg, &s, &big).unwrap_err();
        assert!(err.to_string().contains("spool cap exceeded"), "{err}");
        assert!(err.to_string().contains("Nothing was dropped"), "{err}");
        assert_eq!(list_manifests(tmp.path()).unwrap().len(), 0);
        assert_eq!(dir_usage_bytes(tmp.path()), 0);
    }

    #[test]
    fn list_manifests_on_missing_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        assert!(list_manifests(&missing).unwrap().is_empty());
    }
}
