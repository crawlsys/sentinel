//! `SpentNonceLog` -- in-process (+ optional disk-persistent) set
//! of ChallengeNonce values that have already authorized at least
//! one CatastrophicAck. handle_inbound consults the log BEFORE
//! recording approval; if the witness's challenge_nonce is already
//! in the spent set, the ack is rejected as a replay.
//!
//! # Threat model
//!
//! Without this log, an attacker who intercepts a valid
//! CatastrophicAck on the wire (or replays one from disk via a
//! daemon-restart oracle) could re-present it to authorize a
//! second action. The Praefectus 6-step verification includes a
//! replay check (step 5: spend ledger), but sentinel SHOULD enforce
//! the same locally so a misconfigured Praefectus (or no Praefectus
//! at all in the v0.1 daemon-local trust model) doesn't degrade
//! the property.
//!
//! # Semantics
//!
//! - `insert(nonce)` returns `true` when newly spent (first time),
//!   `false` when already spent (replay). The caller treats `false`
//!   as a rejection.
//! - TTL-evicted. Default 5 min matches the CatastrophicApprovalCache
//!   TTL: a witness whose approval has already expired can't
//!   authorize anything anyway, so retaining its nonce record adds
//!   no security.
//! - Persistent variant survives daemon restart: an attacker who
//!   stashes a CatastrophicAck and replays it after restart still
//!   hits the spent set (the JSONL snapshot was rehydrated).

#![allow(clippy::missing_const_for_fn, clippy::incompatible_msrv)]

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use consul_domain::identity::republic::ChallengeNonce;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Default TTL for spent-nonce records. Matches
/// `CatastrophicApprovalCache::DEFAULT_TTL`.
pub const DEFAULT_TTL: Duration = Duration::from_mins(5);

/// Default daemon-global path for the persistent spent-nonce log.
#[must_use]
pub fn default_spent_nonce_log_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("legatus-spent-nonces.json"),
    )
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    entries: Vec<SnapshotEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotEntry {
    nonce_hex: String,
    spent_at: DateTime<Utc>,
}

const SNAPSHOT_VERSION: u32 = 1;

/// Cheaply cloneable. Internally `Arc<Mutex<HashMap<...>>>`.
#[derive(Clone, Debug, Default)]
pub struct SpentNonceLog {
    inner: Arc<Mutex<HashMap<ChallengeNonce, DateTime<Utc>>>>,
    ttl: Duration,
    persistence_path: Option<PathBuf>,
}

impl SpentNonceLog {
    /// Construct in-memory only with the default 5-min TTL.
    #[must_use]
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TTL)
    }

    /// Construct with a custom TTL.
    #[must_use]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            persistence_path: None,
        }
    }

    /// Construct backed by a JSON snapshot file. Missing /
    /// unreadable / corrupt file → start empty + log warn.
    #[must_use]
    pub fn with_persistence(path: PathBuf, ttl: Duration) -> Self {
        let inner = match load_snapshot(&path) {
            Ok(m) => m,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "could not load spent-nonce snapshot; starting empty"
                );
                HashMap::new()
            }
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            ttl,
            persistence_path: Some(path),
        }
    }

    /// Convenience: persistence-backed log with the default
    /// daemon path + default TTL.
    #[must_use]
    pub fn default_persistent() -> Option<Self> {
        Some(Self::with_persistence(default_spent_nonce_log_path()?, DEFAULT_TTL))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<ChallengeNonce, DateTime<Utc>>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Attempt to spend `nonce`. Returns:
    /// - `true` when the nonce was newly spent (caller proceeds)
    /// - `false` when the nonce was already spent (caller REJECTS
    ///   the witness as a replay)
    ///
    /// Eviction of expired entries happens inside this call.
    pub fn try_spend(&self, nonce: ChallengeNonce) -> bool {
        let mut g = self.lock();
        self.evict_expired(&mut g);
        if g.contains_key(&nonce) {
            return false;
        }
        g.insert(nonce, Utc::now());
        self.snapshot_locked(&g);
        true
    }

    /// Count of currently-valid (post-eviction) spent records. For
    /// diagnostics + tests.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut g = self.lock();
        self.evict_expired(&mut g);
        g.len()
    }

    /// Convenience.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn evict_expired(
        &self,
        g: &mut MutexGuard<'_, HashMap<ChallengeNonce, DateTime<Utc>>>,
    ) {
        let now = Utc::now();
        let ttl = match chrono::Duration::from_std(self.ttl) {
            Ok(d) => d,
            Err(_) => return,
        };
        g.retain(|_, recorded_at| now - *recorded_at < ttl);
    }

    fn snapshot_locked(
        &self,
        g: &MutexGuard<'_, HashMap<ChallengeNonce, DateTime<Utc>>>,
    ) {
        let Some(path) = self.persistence_path.as_ref() else {
            return;
        };
        if let Err(err) = write_snapshot(path, g) {
            warn!(
                path = %path.display(),
                error = %err,
                "spent-nonce snapshot write failed; in-memory state preserved"
            );
        }
    }
}

fn load_snapshot(path: &Path) -> Result<HashMap<ChallengeNonce, DateTime<Utc>>, std::io::Error> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let file = OpenOptions::new().read(true).open(path)?;
    FileExt::lock_shared(&file)?;
    let result = (|| -> Result<HashMap<_, _>, std::io::Error> {
        let snapshot: SnapshotFile = serde_json::from_reader(&file)
            .map_err(|e| std::io::Error::other(format!("parse snapshot: {e}")))?;
        if snapshot.version != SNAPSHOT_VERSION {
            return Err(std::io::Error::other(format!(
                "snapshot version {} unsupported (expected {SNAPSHOT_VERSION})",
                snapshot.version
            )));
        }
        let mut out = HashMap::with_capacity(snapshot.entries.len());
        for entry in snapshot.entries {
            let bytes = hex::decode(&entry.nonce_hex)
                .map_err(|e| std::io::Error::other(format!("decode nonce hex: {e}")))?;
            if bytes.len() != 16 {
                return Err(std::io::Error::other(format!(
                    "nonce hex decoded to {} bytes, expected 16",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&bytes);
            out.insert(ChallengeNonce::from_bytes(arr), entry.spent_at);
        }
        Ok(out)
    })();
    let _ = FileExt::unlock(&file);
    result
}

fn write_snapshot(
    path: &Path,
    map: &HashMap<ChallengeNonce, DateTime<Utc>>,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entries: Vec<SnapshotEntry> = map
        .iter()
        .map(|(n, t)| SnapshotEntry {
            nonce_hex: n.to_hex(),
            spent_at: *t,
        })
        .collect();
    let snapshot = SnapshotFile {
        version: SNAPSHOT_VERSION,
        entries,
    };
    let body = serde_json::to_vec_pretty(&snapshot)
        .map_err(|e| std::io::Error::other(format!("serialize snapshot: {e}")))?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    FileExt::lock_exclusive(&file)?;
    let result = (|| -> Result<(), std::io::Error> {
        file.write_all(&body)?;
        file.sync_all()?;
        Ok(())
    })();
    let _ = FileExt::unlock(&file);
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn nonce(byte: u8) -> ChallengeNonce {
        ChallengeNonce::from_bytes([byte; 16])
    }

    #[test]
    fn fresh_nonce_is_spent_successfully() {
        let log = SpentNonceLog::new();
        assert!(log.try_spend(nonce(0xAA)));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn replayed_nonce_is_rejected() {
        let log = SpentNonceLog::new();
        assert!(log.try_spend(nonce(0xAA)));
        assert!(!log.try_spend(nonce(0xAA)), "second spend should reject");
    }

    #[test]
    fn different_nonces_are_independent() {
        let log = SpentNonceLog::new();
        assert!(log.try_spend(nonce(0xAA)));
        assert!(log.try_spend(nonce(0xBB)));
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn expired_nonce_can_be_respent() {
        let log = SpentNonceLog::with_ttl(Duration::from_millis(1));
        assert!(log.try_spend(nonce(0xAA)));
        std::thread::sleep(Duration::from_millis(10));
        // After TTL the nonce is evicted; a second spend re-uses
        // the slot. (Acceptable per the design: if the matching
        // approval has also expired, no security property is lost.)
        assert!(log.try_spend(nonce(0xAA)));
    }

    fn tempfile_path(name: &str) -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{name}.json"));
        std::mem::forget(dir);
        path
    }

    #[test]
    fn persistent_spent_nonces_survive_recreate() {
        let path = tempfile_path("spent");
        {
            let log = SpentNonceLog::with_persistence(path.clone(), DEFAULT_TTL);
            assert!(log.try_spend(nonce(0xAA)));
            assert!(log.try_spend(nonce(0xBB)));
        }
        // Re-open. Replay the same nonce → still rejected.
        let log2 = SpentNonceLog::with_persistence(path, DEFAULT_TTL);
        assert_eq!(log2.len(), 2);
        assert!(!log2.try_spend(nonce(0xAA)));
        assert!(!log2.try_spend(nonce(0xBB)));
    }

    #[test]
    fn persistent_missing_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-existed.json");
        let log = SpentNonceLog::with_persistence(path, DEFAULT_TTL);
        assert!(log.is_empty());
    }

    #[test]
    fn persistent_corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"definitely not json").unwrap();
        let log = SpentNonceLog::with_persistence(path, DEFAULT_TTL);
        assert!(log.is_empty());
    }
}
