//! `CatastrophicApprovalCache` -- in-process map of
//! `(SessionId, action_class)` -> approval entry, populated by the
//! inbound `CatastrophicAck` handler and consumed by the
//! `catastrophic_escalation` PreToolUse hook on retry.
//!
//! # Architectural fit
//!
//! Per the user-set boundary, sentinel/legatus owns the
//! communication seam, not voice. This cache is the seam: it
//! buffers approvals that arrived from the consul side until the
//! operator's NEXT Claude Code prompt re-triggers the
//! catastrophic tool call, at which point the hook drains the
//! pertinent approval and allows the action through.
//!
//! # Semantics
//!
//! - One approval per `(SessionId, action_class)`. The action_class
//!   v0.1 is loose: derived from the witness transcript by parsing
//!   "approve <action_class>, code <nonce>".
//! - Single-use: `consume` removes the entry on read so the same
//!   approval cannot authorize two retries.
//! - TTL-evicted: entries older than `DEFAULT_TTL` are dropped on
//!   any access. Protects against a stale approval auto-allowing a
//!   stale retry hours later.
//! - In-process only: no on-disk persistence. A daemon restart
//!   loses pending approvals; the operator re-authorizes.

#![allow(clippy::missing_const_for_fn, clippy::incompatible_msrv)]

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use consul_domain::identity::SessionId;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Default daemon-global path for the persistent approval cache
/// snapshot. Returns `None` only when `dirs::home_dir()` is
/// unresolvable.
#[must_use]
pub fn default_approval_cache_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("legatus-catastrophic-approvals.json"),
    )
}

/// Default TTL for cached approvals. Long enough for the operator
/// to switch context back to Claude Code and retry the action,
/// short enough that a forgotten approval can't authorize a much-
/// later retry.
pub const DEFAULT_TTL: Duration = Duration::from_mins(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalEntry {
    /// Wall-clock (UTC) when the approval was recorded. Wall-clock
    /// rather than Instant so the entry survives serialization
    /// across daemon restarts. TTL eviction uses
    /// `Utc::now() - recorded_at` -- slightly clock-skew-sensitive
    /// but acceptable for a 5-minute window.
    recorded_at: DateTime<Utc>,
    /// Audit-log breadcrumb: the operator's spoken transcript that
    /// produced this approval. Surfaced in the hook's allow
    /// message so the audit trail shows what was approved.
    transcript: String,
}

/// On-disk snapshot format. Versioned for forward compatibility.
#[derive(Debug, Serialize, Deserialize)]
struct SnapshotFile {
    version: u32,
    entries: Vec<SnapshotEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SnapshotEntry {
    session_id: SessionId,
    action_class: String,
    recorded_at: DateTime<Utc>,
    transcript: String,
}

const SNAPSHOT_VERSION: u32 = 1;

/// Cheaply cloneable approval cache. Internally `Arc<Mutex<_>>`.
///
/// Persistence is opt-in via [`CatastrophicApprovalCache::with_persistence`].
/// When persistence is enabled, every record/consume/eviction
/// writes a JSON snapshot to disk under an fs2 advisory exclusive
/// lock; the next process restart loads from the same file.
#[derive(Clone, Debug, Default)]
pub struct CatastrophicApprovalCache {
    inner: Arc<Mutex<HashMap<(SessionId, String), ApprovalEntry>>>,
    ttl: Duration,
    /// When `Some(path)`, every mutation snapshots the full
    /// HashMap to `path` (write + advisory exclusive lock). When
    /// `None`, behaves as an in-memory cache (the v0.1 default).
    persistence_path: Option<PathBuf>,
}

/// Returned by `consume`: the approval found (and removed), or
/// `None` if absent / expired.
#[derive(Debug, Clone)]
pub struct ConsumedApproval {
    /// Approving operator's transcript, captured verbatim from
    /// the witness for audit.
    pub transcript: String,
    /// How long ago the approval was recorded (for diagnostics).
    pub age: Duration,
}

impl CatastrophicApprovalCache {
    /// Construct with the default 5-minute TTL.
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

    /// Construct backed by a JSON snapshot file. The file is
    /// loaded at construction (missing/unreadable file -> empty
    /// cache, logged at warn). Every subsequent mutation rewrites
    /// the full snapshot under an fs2 advisory exclusive lock.
    ///
    /// Use [`default_approval_cache_path`] for the conventional
    /// path under `~/.claude/sentinel/state/`.
    #[must_use]
    pub fn with_persistence(path: PathBuf, ttl: Duration) -> Self {
        let inner = match load_snapshot(&path) {
            Ok(map) => map,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "could not load approval cache snapshot; starting empty"
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

    /// Convenience: persistence-backed cache with the default
    /// daemon path + default TTL.
    #[must_use]
    pub fn default_persistent() -> Option<Self> {
        Some(Self::with_persistence(
            default_approval_cache_path()?,
            DEFAULT_TTL,
        ))
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<(SessionId, String), ApprovalEntry>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record an approval that arrived via `CatastrophicAck`.
    pub fn record(&self, session_id: SessionId, action_class: String, transcript: String) {
        let mut g = self.lock();
        self.evict_expired(&mut g);
        g.insert(
            (session_id, normalize(&action_class)),
            ApprovalEntry {
                recorded_at: Utc::now(),
                transcript,
            },
        );
        self.snapshot_locked(&g);
    }

    /// Consume an approval matching `(session_id, action_class)`.
    /// Returns the consumed approval or `None` if no fresh
    /// approval is present. Single-use: a successful consume
    /// removes the entry.
    pub fn consume(
        &self,
        session_id: SessionId,
        action_class: &str,
    ) -> Option<ConsumedApproval> {
        let mut g = self.lock();
        self.evict_expired(&mut g);
        let key = (session_id, normalize(action_class));
        let entry = g.remove(&key)?;
        let age = (Utc::now() - entry.recorded_at)
            .to_std()
            .unwrap_or(Duration::ZERO);
        self.snapshot_locked(&g);
        Some(ConsumedApproval {
            transcript: entry.transcript,
            age,
        })
    }

    /// Count of currently-valid (post-eviction) entries. For
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
        g: &mut MutexGuard<'_, HashMap<(SessionId, String), ApprovalEntry>>,
    ) {
        let now = Utc::now();
        let ttl = match chrono::Duration::from_std(self.ttl) {
            Ok(d) => d,
            Err(_) => return, // TTL too large to express in chrono; skip.
        };
        g.retain(|_, entry| now - entry.recorded_at < ttl);
    }

    /// Write the current snapshot to disk if persistence is
    /// configured. Errors are logged + swallowed -- a transient
    /// disk failure must not lose in-memory state.
    fn snapshot_locked(
        &self,
        g: &MutexGuard<'_, HashMap<(SessionId, String), ApprovalEntry>>,
    ) {
        let Some(path) = self.persistence_path.as_ref() else {
            return;
        };
        if let Err(err) = write_snapshot(path, g) {
            warn!(
                path = %path.display(),
                error = %err,
                "approval cache snapshot write failed; in-memory state preserved"
            );
        }
    }
}

/// Read + parse the JSON snapshot file. Missing file -> empty
/// HashMap. Malformed file -> error (caller logs + starts empty).
fn load_snapshot(
    path: &Path,
) -> Result<HashMap<(SessionId, String), ApprovalEntry>, std::io::Error> {
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
            out.insert(
                (entry.session_id, entry.action_class),
                ApprovalEntry {
                    recorded_at: entry.recorded_at,
                    transcript: entry.transcript,
                },
            );
        }
        Ok(out)
    })();
    let _ = FileExt::unlock(&file);
    result
}

/// Write the snapshot atomically: open with truncate, lock
/// exclusive, write JSON, sync, drop lock. The fs2 advisory
/// lock serializes concurrent processes writing to the same
/// path (e.g. two daemons started accidentally on the same
/// host).
fn write_snapshot(
    path: &Path,
    map: &HashMap<(SessionId, String), ApprovalEntry>,
) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entries: Vec<SnapshotEntry> = map
        .iter()
        .map(|((sid, ac), e)| SnapshotEntry {
            session_id: *sid,
            action_class: ac.clone(),
            recorded_at: e.recorded_at,
            transcript: e.transcript.clone(),
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

/// Normalize an action class for cache-key comparison: trim +
/// lowercase. Tolerates whitespace / capitalization differences
/// between the consul transcript ("approve Bash, ...") and the
/// hook's classifier output ("bash").
fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Parse `action_class` out of a witness transcript shaped like
/// "approve <action_class>, code <nonce>" (case-insensitive,
/// flexible whitespace). Returns `None` if the transcript doesn't
/// match the expected shape. Used by the inbound `CatastrophicAck`
/// handler to decide which `(session, action_class)` slot to
/// approve.
#[must_use]
pub fn parse_action_class_from_transcript(transcript: &str) -> Option<String> {
    let lower = transcript.to_lowercase();
    let approve_idx = lower.find("approve ")?;
    let after_approve = &transcript[approve_idx + "approve ".len()..];
    // action_class runs until the next comma or "code" marker.
    let stop = after_approve
        .find(',')
        .or_else(|| after_approve.to_lowercase().find(" code "))
        .unwrap_or(after_approve.len());
    let candidate = after_approve[..stop].trim();
    if candidate.is_empty() {
        None
    } else {
        Some(candidate.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn sid(byte: u8) -> SessionId {
        SessionId::from_uuid(Uuid::from_bytes([byte; 16]))
    }

    #[test]
    fn record_then_consume_round_trips() {
        let cache = CatastrophicApprovalCache::new();
        cache.record(sid(0xAA), "deploy".into(), "approve deploy, code abc".into());
        let consumed = cache.consume(sid(0xAA), "deploy").unwrap();
        assert!(consumed.transcript.contains("approve deploy"));
    }

    #[test]
    fn consume_is_single_use() {
        let cache = CatastrophicApprovalCache::new();
        cache.record(sid(0xAA), "deploy".into(), "approve deploy".into());
        assert!(cache.consume(sid(0xAA), "deploy").is_some());
        assert!(cache.consume(sid(0xAA), "deploy").is_none());
    }

    #[test]
    fn consume_misses_for_unknown_action_class() {
        let cache = CatastrophicApprovalCache::new();
        cache.record(sid(0xAA), "deploy".into(), "...".into());
        assert!(cache.consume(sid(0xAA), "drop_table").is_none());
        // The original approval remains because we missed.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn consume_misses_for_wrong_session() {
        let cache = CatastrophicApprovalCache::new();
        cache.record(sid(0xAA), "deploy".into(), "...".into());
        assert!(cache.consume(sid(0xBB), "deploy").is_none());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn normalize_is_case_and_whitespace_tolerant() {
        let cache = CatastrophicApprovalCache::new();
        cache.record(sid(0xAA), "Deploy".into(), "...".into());
        // Hook side passes lowercased / different-cased lookup;
        // cache normalizes both sides.
        assert!(cache.consume(sid(0xAA), "  DEPLOY ").is_some());
    }

    #[test]
    fn expired_entries_evicted_on_access() {
        let cache = CatastrophicApprovalCache::with_ttl(Duration::from_millis(1));
        cache.record(sid(0xAA), "deploy".into(), "...".into());
        std::thread::sleep(Duration::from_millis(10));
        assert!(cache.consume(sid(0xAA), "deploy").is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn parse_extracts_action_class_from_standard_transcript() {
        let t = "approve deploy, code 3f2a1b09c8d7";
        assert_eq!(parse_action_class_from_transcript(t).as_deref(), Some("deploy"));
    }

    #[test]
    fn parse_handles_case_insensitive_approve() {
        let t = "Approve Deploy, code 3f2a";
        assert_eq!(parse_action_class_from_transcript(t).as_deref(), Some("Deploy"));
    }

    #[test]
    fn parse_handles_action_class_with_underscores_and_spaces() {
        let t = "approve drop_table users, code abc";
        // Stops at the first comma -> entire phrase up to comma.
        assert_eq!(
            parse_action_class_from_transcript(t).as_deref(),
            Some("drop_table users")
        );
    }

    #[test]
    fn parse_returns_none_for_unrelated_transcript() {
        assert!(parse_action_class_from_transcript("hello world").is_none());
    }

    #[test]
    fn parse_returns_none_for_empty_action_class() {
        // "approve , code ..." (operator garbled it)
        let t = "approve , code 123";
        assert!(parse_action_class_from_transcript(t).is_none());
    }

    // ---- Persistence tests ----

    fn tempfile_path(name: &str) -> PathBuf {
        // Use a per-test directory under tempfile so concurrent
        // tests don't collide on the snapshot file.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{name}.json"));
        // Leak the TempDir so the file outlives this fn -- we
        // don't need cleanup for short-lived tests; the OS reaps.
        std::mem::forget(dir);
        path
    }

    #[test]
    fn persistent_cache_round_trips_across_recreate() {
        let path = tempfile_path("rt");
        {
            let cache = CatastrophicApprovalCache::with_persistence(path.clone(), DEFAULT_TTL);
            cache.record(
                sid(0xAA),
                "deploy".into(),
                "approve deploy, code 1234".into(),
            );
            cache.record(sid(0xBB), "drop_table".into(), "approve drop_table".into());
            assert_eq!(cache.len(), 2);
        }
        // Drop cache + recreate from disk.
        let cache2 = CatastrophicApprovalCache::with_persistence(path, DEFAULT_TTL);
        assert_eq!(cache2.len(), 2);
        let consumed = cache2.consume(sid(0xAA), "deploy").unwrap();
        assert!(consumed.transcript.contains("approve deploy"));
        assert!(cache2.consume(sid(0xBB), "drop_table").is_some());
    }

    #[test]
    fn persistent_cache_consume_is_persisted() {
        let path = tempfile_path("consume_persisted");
        let cache = CatastrophicApprovalCache::with_persistence(path.clone(), DEFAULT_TTL);
        cache.record(sid(0xAA), "deploy".into(), "approve deploy".into());
        assert!(cache.consume(sid(0xAA), "deploy").is_some());
        // Re-open: consumed entry should NOT be present.
        let cache2 = CatastrophicApprovalCache::with_persistence(path, DEFAULT_TTL);
        assert!(cache2.consume(sid(0xAA), "deploy").is_none());
        assert_eq!(cache2.len(), 0);
    }

    #[test]
    fn persistent_cache_missing_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("never-existed.json");
        let cache = CatastrophicApprovalCache::with_persistence(path, DEFAULT_TTL);
        assert!(cache.is_empty());
    }

    #[test]
    fn persistent_cache_corrupt_file_logs_and_starts_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.json");
        std::fs::write(&path, b"not valid json {{").unwrap();
        let cache = CatastrophicApprovalCache::with_persistence(path, DEFAULT_TTL);
        assert!(cache.is_empty(), "corrupt file should yield empty cache");
    }

    #[test]
    fn expired_entries_dropped_on_reload() {
        let path = tempfile_path("ttl");
        {
            let cache = CatastrophicApprovalCache::with_persistence(
                path.clone(),
                Duration::from_millis(1),
            );
            cache.record(sid(0xAA), "deploy".into(), "...".into());
            std::thread::sleep(Duration::from_millis(10));
        }
        // Re-open with the same short TTL; the persisted entry
        // is now older than TTL -> eviction on next access.
        let cache2 =
            CatastrophicApprovalCache::with_persistence(path, Duration::from_millis(1));
        assert_eq!(cache2.len(), 0, "expired entry should be evicted on read");
    }
}
