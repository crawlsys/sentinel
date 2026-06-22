//! Durable collector checkpoints — `(dev, inode) + byte offset` cursors for
//! append-only ledgers, content hashes for snapshot sources (the LEG-261
//! seam).
//!
//! One JSON file (`checkpoint.json`) holds every source's cursor. Writes are
//! atomic (tmp + fsync + rename + best-effort dir fsync) so a crash mid-write
//! can never corrupt the previous checkpoint. The collect engine persists the
//! checkpoint only AFTER the corresponding spool batch is durably on disk —
//! the crash window therefore re-spools an identical batch (same offset →
//! same content → same batch id → overwrite) instead of dropping or
//! duplicating rows.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Current checkpoint file format version.
pub const CHECKPOINT_VERSION: u32 = 1;

/// Stable identity of an on-disk file across renames. Sentinel's metrics
/// rotation renames the live ledger to `<name>.archive.<ts_ms>` — the inode
/// follows the rename, so a cursor keyed by identity survives rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    /// Identity from file metadata. On unix this is the real `(dev, inode)`
    /// pair. On non-unix targets we fall back to creation-time nanos (a
    /// rename preserves creation time, so rotation detection still works).
    #[must_use]
    pub fn of(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let nanos = meta
                .created()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
            Self { dev: 0, ino: nanos }
        }
    }
}

/// Cursor into one append-only ledger file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerCursor {
    pub dev: u64,
    pub ino: u64,
    /// Byte offset of the first unconsumed byte (always at a line boundary —
    /// partial trailing lines are never consumed).
    pub offset: u64,
}

impl LedgerCursor {
    /// The file-identity half of the cursor.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        FileIdentity {
            dev: self.dev,
            ino: self.ino,
        }
    }
}

/// Cursor for a snapshot-style source (ship-on-hash-change). Unused by the
/// ledger collector — this is the seam the LEG-261 KPI/rollup adapters plug
/// into without a checkpoint-format migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotCursor {
    /// Hex sha256 of the last spooled snapshot content.
    pub sha256: String,
    /// RFC3339 capture time of the last spooled snapshot.
    pub captured_at: String,
}

/// The full checkpoint state — every source's cursor, one file on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(default)]
    pub version: u32,
    /// Append-only ledger cursors keyed by source name (`claude`, `codex`, …).
    #[serde(default)]
    pub ledgers: BTreeMap<String, LedgerCursor>,
    /// Snapshot-source cursors keyed by source name (LEG-261 seam).
    #[serde(default)]
    pub snapshots: BTreeMap<String, SnapshotCursor>,
}

/// Loads, mutates, and atomically persists the checkpoint file.
#[derive(Debug)]
pub struct CheckpointStore {
    path: PathBuf,
    checkpoint: Checkpoint,
}

impl CheckpointStore {
    /// Load the checkpoint at `path`. A missing file yields an empty
    /// checkpoint (first run). A corrupt file is an error — silently
    /// restarting from scratch would re-spool history.
    pub fn load(path: &Path) -> Result<Self> {
        let checkpoint = if path.exists() {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("read checkpoint {}", path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parse checkpoint {}", path.display()))?
        } else {
            Checkpoint {
                version: CHECKPOINT_VERSION,
                ..Checkpoint::default()
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            checkpoint,
        })
    }

    #[must_use]
    pub const fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    #[must_use]
    pub fn ledger(&self, source: &str) -> Option<LedgerCursor> {
        self.checkpoint.ledgers.get(source).copied()
    }

    pub fn set_ledger(&mut self, source: &str, cursor: LedgerCursor) {
        self.checkpoint.ledgers.insert(source.to_string(), cursor);
    }

    #[must_use]
    pub fn snapshot(&self, source: &str) -> Option<&SnapshotCursor> {
        self.checkpoint.snapshots.get(source)
    }

    pub fn set_snapshot(&mut self, source: &str, cursor: SnapshotCursor) {
        self.checkpoint.snapshots.insert(source.to_string(), cursor);
    }

    /// Atomically persist the checkpoint: write `<file>.tmp`, fsync, rename
    /// over the live file, then best-effort fsync the parent dir so the
    /// rename itself is durable.
    pub fn persist(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("checkpoint path has no parent dir"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create checkpoint dir {}", parent.display()))?;

        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&self.checkpoint).context("encode checkpoint")?;
        {
            let mut f =
                fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(json.as_bytes())
                .with_context(|| format!("write {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("fsync {}", tmp.display()))?;
        }
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), self.path.display()))?;
        // Dir fsync makes the rename durable; best-effort (not all platforms
        // allow opening a directory for sync).
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_empty_and_persist_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("telemetry").join("checkpoint.json");

        let mut store = CheckpointStore::load(&path).unwrap();
        assert!(store.checkpoint().ledgers.is_empty());
        assert_eq!(store.checkpoint().version, CHECKPOINT_VERSION);

        store.set_ledger(
            "claude",
            LedgerCursor {
                dev: 7,
                ino: 42,
                offset: 1234,
            },
        );
        store.set_snapshot(
            "roi-summary",
            SnapshotCursor {
                sha256: "ab".repeat(32),
                captured_at: "2026-06-12T00:00:00Z".to_string(),
            },
        );
        store.persist().unwrap();

        let reloaded = CheckpointStore::load(&path).unwrap();
        assert_eq!(
            reloaded.ledger("claude"),
            Some(LedgerCursor {
                dev: 7,
                ino: 42,
                offset: 1234,
            })
        );
        assert_eq!(
            reloaded.snapshot("roi-summary").unwrap().captured_at,
            "2026-06-12T00:00:00Z"
        );
        // No stray tmp file left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn corrupt_checkpoint_is_an_error_not_a_silent_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("checkpoint.json");
        fs::write(&path, b"{not json").unwrap();
        assert!(CheckpointStore::load(&path).is_err());
    }

    #[test]
    fn identity_survives_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("ledger.jsonl");
        fs::write(&a, b"x\n").unwrap();
        let before = FileIdentity::of(&fs::metadata(&a).unwrap());
        let b = tmp.path().join("ledger.jsonl.archive.123");
        fs::rename(&a, &b).unwrap();
        let after = FileIdentity::of(&fs::metadata(&b).unwrap());
        assert_eq!(before, after);
    }
}
