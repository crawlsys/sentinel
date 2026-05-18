//! BA1 Phase 4a — JSONL-backed provenance store.
//!
//! Implements BOTH [`ProvenancePort`] (read) and
//! [`ProvenanceWritePort`] (write) over a single append-only JSONL
//! file at `~/.claude/sentinel/state/provenance/records.jsonl`. The
//! [`audit_extract`](sentinel_application::hooks::audit_extract)
//! hook writes; the
//! [`provenance_validate`](sentinel_application::hooks::provenance_validate)
//! hook reads. Same backing file → no race window between "lift
//! emitted" and "validate sees the record."
//!
//! Mirrors the [`JsonlAppraisalStore`](crate::appraisal_store::JsonlAppraisalStore)
//! pattern from A2 Phase 3c: POSIX-atomic appends via `O_APPEND`,
//! malformed-line tolerance (skip + log, don't poison the file),
//! auto-truncation at [`MAX_LOG_SIZE`] (keep trailing half via
//! atomic tmp+rename rewrite). Best-effort persistence — write
//! failures emit a `tracing::warn` but never propagate as a `Block`
//! per the BA1 design contract (hooks are observational w.r.t. the
//! storage layer).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use sentinel_domain::ba::RetrievalRecord;
use sentinel_domain::ports::{ProvenanceError, ProvenancePort, ProvenanceWritePort};

/// Per-file size cap (8 MB ≈ `50_000` records at typical sizes).
/// Beyond this, the next `record()` call truncates to the trailing
/// half via atomic tmp+rename rewrite.
pub const MAX_LOG_SIZE: u64 = 8 * 1024 * 1024;

/// JSONL-backed [`ProvenancePort`] + [`ProvenanceWritePort`] adapter.
///
/// Append-only writes; cross-session-visible reads. Thread-safe via
/// an internal [`Mutex`] guarding the truncate-rewrite path only —
/// concurrent appends are POSIX-atomic and don't need the mutex.
pub struct JsonlProvenanceStore {
    path: PathBuf,
    /// Guard for the truncation rewrite path. Held only during
    /// rewrite; appends don't take it.
    truncate_guard: Mutex<()>,
}

impl std::fmt::Debug for JsonlProvenanceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlProvenanceStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl JsonlProvenanceStore {
    /// Construct pointed at a specific file. Used by tests to scope
    /// to a tempdir.
    #[must_use]
    pub const fn at_path(path: PathBuf) -> Self {
        Self {
            path,
            truncate_guard: Mutex::new(()),
        }
    }

    /// Construct with the default path
    /// (`~/.claude/sentinel/state/provenance/records.jsonl`). Errors
    /// when `dirs::home_dir()` fails.
    pub fn with_default_path() -> Result<Self> {
        let home =
            dirs::home_dir().context("home directory not resolvable from environment")?;
        let path = home
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join("provenance")
            .join("records.jsonl");
        Ok(Self::at_path(path))
    }

    /// Read-only access to the file path. Useful for tests + operator
    /// reporting tooling.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read every record from the file. Lines that fail to parse are
    /// skipped + logged at `warn` level. Returns an empty `Vec` when
    /// the file doesn't exist yet.
    fn read_all_records(&self) -> Vec<RetrievalRecord> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                tracing::warn!(?err, path = ?self.path, "failed to read provenance records");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(content.lines().count());
        for (idx, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<RetrievalRecord>(line) {
                Ok(r) => out.push(r),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        path = ?self.path,
                        line_number = idx + 1,
                        "skipping unparseable provenance record"
                    );
                }
            }
        }
        out
    }

    /// Truncate the file in-place by keeping only the trailing half
    /// of records. Called from `record()` when the file exceeds
    /// [`MAX_LOG_SIZE`].
    fn maybe_truncate(&self) {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return;
        };
        if metadata.len() <= MAX_LOG_SIZE {
            return;
        }
        let Ok(_guard) = self.truncate_guard.lock() else {
            tracing::warn!(
                path = ?self.path,
                "provenance store truncate guard poisoned; skipping rewrite"
            );
            return;
        };
        // Double-check inside the lock — another thread may have
        // already truncated.
        if let Ok(meta) = std::fs::metadata(&self.path) {
            if meta.len() <= MAX_LOG_SIZE {
                return;
            }
        }
        let all = self.read_all_records();
        let keep = all.len() / 2;
        let trimmed = &all[all.len().saturating_sub(keep)..];
        let mut new_content =
            String::with_capacity(usize::try_from(MAX_LOG_SIZE / 2).unwrap_or(usize::MAX));
        for r in trimmed {
            match serde_json::to_string(r) {
                Ok(line) => {
                    new_content.push_str(&line);
                    new_content.push('\n');
                }
                Err(err) => {
                    tracing::warn!(?err, "skipping unserializable record during truncate");
                }
            }
        }
        let tmp = self.path.with_extension("jsonl.truncate-tmp");
        if let Err(err) = std::fs::write(&tmp, new_content.as_bytes()) {
            tracing::warn!(?err, path = ?tmp, "failed to write truncate tmp file");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            tracing::warn!(?err, path = ?self.path, "failed to rename truncate tmp into place");
        }
    }

    /// Append a record without truncation check. Used internally by
    /// `record()` and by tests that want deterministic writes.
    fn append_raw(&self, record: &RetrievalRecord) -> Result<(), ProvenanceError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ProvenanceError::StoreUnavailable(format!(
                    "failed to create parent dir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let line = serde_json::to_string(record).map_err(|e| {
            ProvenanceError::Malformed(format!("failed to serialize retrieval record: {e}"))
        })?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| {
                ProvenanceError::StoreUnavailable(format!(
                    "failed to open {}: {e}",
                    self.path.display()
                ))
            })?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|e| {
                ProvenanceError::StoreUnavailable(format!(
                    "failed to append to {}: {e}",
                    self.path.display()
                ))
            })?;
        Ok(())
    }
}

impl ProvenancePort for JsonlProvenanceStore {
    fn query_artifact_history(
        &self,
        artifact_id: &str,
    ) -> Result<Vec<RetrievalRecord>, ProvenanceError> {
        let all = self.read_all_records();
        Ok(all
            .into_iter()
            .filter(|r| r.artifact_id == artifact_id)
            .collect())
    }
}

impl ProvenanceWritePort for JsonlProvenanceStore {
    fn record(&self, record: RetrievalRecord) -> Result<(), ProvenanceError> {
        let result = self.append_raw(&record);
        if result.is_ok() {
            self.maybe_truncate();
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sentinel_domain::ba::ProvenanceClass;
    use tempfile::TempDir;

    fn record(artifact_id: &str, content_hash: &str, session: &str) -> RetrievalRecord {
        RetrievalRecord {
            artifact_id: artifact_id.to_string(),
            connector_name: "mcp__linear__get_issue".to_string(),
            content_hash: content_hash.to_string(),
            provenance_class: ProvenanceClass::SystemOfRecord,
            session_id: session.to_string(),
            retrieved_at: Utc::now(),
        }
    }

    fn store(dir: &TempDir) -> JsonlProvenanceStore {
        JsonlProvenanceStore::at_path(dir.path().join("records.jsonl"))
    }

    // ---- Missing-file semantics ----

    #[test]
    fn missing_file_returns_empty_history() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let history = s.query_artifact_history("FIR-123").unwrap();
        assert!(history.is_empty());
    }

    // ---- Write + read round-trip ----

    #[test]
    fn record_then_query_returns_matching_records() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.record(record("FIR-123", "h1", "s1")).unwrap();
        s.record(record("FIR-123", "h2", "s2")).unwrap();
        let history = s.query_artifact_history("FIR-123").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content_hash, "h1");
        assert_eq!(history[1].content_hash, "h2");
    }

    #[test]
    fn record_auto_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("deep").join("nest").join("records.jsonl");
        let s = JsonlProvenanceStore::at_path(nested.clone());
        s.record(record("FIR-1", "h", "s1")).unwrap();
        assert!(nested.exists(), "parent directory must be auto-created");
    }

    // ---- Segregation by artifact_id ----

    #[test]
    fn query_filters_by_artifact_id() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.record(record("FIR-A", "ha", "s1")).unwrap();
        s.record(record("FIR-B", "hb", "s1")).unwrap();
        s.record(record("FIR-A", "ha2", "s1")).unwrap();
        let a = s.query_artifact_history("FIR-A").unwrap();
        let b = s.query_artifact_history("FIR-B").unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 1);
        assert!(a.iter().all(|r| r.artifact_id == "FIR-A"));
        assert!(b.iter().all(|r| r.artifact_id == "FIR-B"));
    }

    #[test]
    fn query_returns_empty_for_unknown_artifact() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        s.record(record("FIR-A", "h", "s1")).unwrap();
        let history = s.query_artifact_history("FIR-NONEXISTENT").unwrap();
        assert!(history.is_empty());
    }

    // ---- Cross-instance persistence ----

    #[test]
    fn second_instance_sees_persisted_records() {
        let dir = TempDir::new().unwrap();
        {
            let s1 = store(&dir);
            s1.record(record("FIR-1", "h1", "s1")).unwrap();
            s1.record(record("FIR-1", "h2", "s2")).unwrap();
        }
        let s2 = JsonlProvenanceStore::at_path(dir.path().join("records.jsonl"));
        let history = s2.query_artifact_history("FIR-1").unwrap();
        assert_eq!(history.len(), 2, "records persist across instances");
    }

    // ---- Both ports same instance ----

    #[test]
    fn single_instance_implements_both_read_and_write_ports() {
        // The whole point of Phase 4a: one struct, both traits, same
        // file. Confirm a single instance can play both roles.
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        // Use through the write port:
        let writer: &dyn ProvenanceWritePort = &s;
        writer.record(record("FIR-1", "h", "s1")).unwrap();
        // Use through the read port:
        let reader: &dyn ProvenancePort = &s;
        let history = reader.query_artifact_history("FIR-1").unwrap();
        assert_eq!(history.len(), 1);
    }

    // ---- Malformed-line tolerance ----

    #[test]
    fn malformed_lines_are_skipped_not_propagated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("records.jsonl");
        let s = JsonlProvenanceStore::at_path(path.clone());
        s.record(record("FIR-1", "h1", "s1")).unwrap();
        // Inject a corrupt line:
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"this is not json\n")
            .unwrap();
        s.record(record("FIR-1", "h2", "s1")).unwrap();
        let history = s.query_artifact_history("FIR-1").unwrap();
        assert_eq!(
            history.len(),
            2,
            "valid records still counted despite the corrupt line between them"
        );
    }

    // ---- Auto-truncation ----

    #[test]
    fn truncate_keeps_trailing_half_when_oversized() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("records.jsonl");
        let s = JsonlProvenanceStore::at_path(path.clone());
        // Write some valid records first.
        for i in 0..100 {
            s.record(record("FIR-1", &format!("hash-{i}"), "s1")).unwrap();
        }
        // Force the file past MAX_LOG_SIZE by padding it.
        let padding =
            "x".repeat(usize::try_from(MAX_LOG_SIZE).unwrap_or(usize::MAX) + 1024);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(padding.as_bytes())
            .unwrap();
        // Next record() triggers truncation.
        s.record(record("FIR-1", "post-trunc", "s1")).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(
            metadata.len() < MAX_LOG_SIZE,
            "file size should drop below MAX_LOG_SIZE post-truncate; got {}",
            metadata.len()
        );
        // Should still have some records readable.
        let history = s.query_artifact_history("FIR-1").unwrap();
        assert!(!history.is_empty(), "post-truncate read should still have records");
    }

    // ---- Path semantics ----

    #[test]
    fn with_default_path_resolves_under_claude_dir() {
        let s = JsonlProvenanceStore::with_default_path().unwrap();
        let p = s.path().display().to_string();
        assert!(
            p.contains(".claude")
                && p.contains("sentinel")
                && p.contains("provenance")
                && p.ends_with("records.jsonl"),
            "default path should live under .claude/sentinel/state/provenance/records.jsonl, got {p}"
        );
    }

    // ---- Send + Sync ----

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsonlProvenanceStore>();
    }
}
