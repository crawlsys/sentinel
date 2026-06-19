//! A2 Phase 3c — JSONL-backed `AppraisalStorePort` adapter.
//!
//! Persists [`AppraisalRecord`]s to a single append-only JSONL file at
//! `~/.claude/sentinel/state/appraisal/records.jsonl`. Production
//! complement to the in-memory store in
//! [`sentinel_application::appraisal_store::InMemoryAppraisalStore`] —
//! same trait, persistent + cross-session-visible storage.
//!
//! ## R5 quarantine boundary
//!
//! Appraisal records are *dispatch input*, never *training signal*.
//! The router reads aggregated stats to pick agents; agents must NOT
//! see appraisal data as feedback. The contract is on the port
//! ([`AppraisalStorePort`]), enforced naturally by the trait surface
//! (only `record` + `aggregate` are exposed — no "stream back to
//! agent for fine-tuning" method exists).
//!
//! ## Concurrency
//!
//! Each `record()` opens the file with `O_APPEND` and writes a single
//! JSONL line (followed by `\n`). POSIX guarantees atomic writes ≤
//! `PIPE_BUF` (4096 bytes on most systems) when `O_APPEND` is set, so
//! concurrent appends from multiple processes interleave at line
//! boundaries — no file locking required. Lines that exceed
//! `PIPE_BUF` aren't atomic; in that rare case the reader's
//! `serde_json::from_str` will fail on the malformed line and skip it
//! (warned, not propagated). For our auditor-verdict-sized records
//! this never trips.
//!
//! ## Auto-truncation
//!
//! When the file exceeds [`MAX_LOG_SIZE`] (8 MB), the next `record()`
//! call rewrites the file keeping only the trailing half. Bounds
//! growth without losing recent data. Mirrors the pattern in
//! [`crate::activity_log`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};

use sentinel_domain::agent_routing::{
    AggregateStats, AppraisalRecord, AppraisalWindow, RequirementSignature,
};
use sentinel_domain::capability::AgentId;
use sentinel_domain::ports::AppraisalStorePort;

/// Per-file size cap.
///
/// Roughly 8 MB ≈ `50_000` records at typical sizes — well past the
/// router's 200-record window and past anything a single operator
/// would query.
pub const MAX_LOG_SIZE: u64 = 8 * 1024 * 1024;

/// JSONL-backed [`AppraisalStorePort`] implementation.
///
/// Append-only writes; cross-session-visible reads. Thread-safe via
/// an internal [`Mutex`] guarding the read-then-rewrite truncation
/// path (concurrent appends are POSIX-atomic so they don't need the
/// mutex; truncate-and-rewrite does).
pub struct JsonlAppraisalStore {
    path: PathBuf,
    /// Guard for the truncation rewrite path. Held only during
    /// rewrite — `record()`'s append fast path doesn't take it.
    truncate_guard: Mutex<()>,
}

impl std::fmt::Debug for JsonlAppraisalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlAppraisalStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl JsonlAppraisalStore {
    /// Construct from a specific file path. Used by tests to point
    /// the store at a tempdir.
    #[must_use]
    pub const fn at_path(path: PathBuf) -> Self {
        Self {
            path,
            truncate_guard: Mutex::new(()),
        }
    }

    /// Construct with the default path
    /// (`~/.claude/sentinel/state/appraisal/records.jsonl`).
    pub fn with_default_path() -> Result<Self> {
        let path = crate::state_store::state_dir()
            .join("appraisal")
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
    /// skipped + logged at `warn` level (so a partial / corrupt line
    /// doesn't poison the aggregate query). Returns an empty `Vec`
    /// when the file doesn't yet exist.
    fn read_all_records(&self) -> Vec<AppraisalRecord> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                tracing::warn!(?err, path = ?self.path, "failed to read appraisal records");
                return Vec::new();
            }
        };
        let mut out = Vec::with_capacity(content.lines().count());
        for (idx, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<AppraisalRecord>(line) {
                Ok(r) => out.push(r),
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        path = ?self.path,
                        line_number = idx + 1,
                        "skipping unparseable appraisal record"
                    );
                }
            }
        }
        out
    }

    /// Truncate the file in-place by keeping only the trailing half
    /// of records. Called from `record()` when the file exceeds
    /// [`MAX_LOG_SIZE`]. Acquires the truncate guard to serialize
    /// concurrent rewrites; if the guard is poisoned (panic in
    /// another thread), the call returns without rewriting and the
    /// file grows past the cap until the next call recovers.
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
                "appraisal store truncate guard poisoned; skipping rewrite"
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
        // Atomic rewrite via tmp-file + rename so a crash mid-write
        // doesn't truncate to zero.
        let tmp = self.path.with_extension("jsonl.truncate-tmp");
        if let Err(err) = std::fs::write(&tmp, new_content.as_bytes()) {
            tracing::warn!(?err, path = ?tmp, "failed to write truncate tmp file");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            tracing::warn!(?err, path = ?self.path, "failed to rename truncate tmp into place");
        }
    }

    /// Append a record without checking size. Used by tests that want
    /// deterministic writes without truncation side effects.
    fn append_raw(&self, record: &AppraisalRecord) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create appraisal dir {}", parent.display()))?;
        }
        let line = serde_json::to_string(record)
            .context("failed to serialize appraisal record to JSON")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        file.write_all(line.as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .with_context(|| {
                format!("failed to write appraisal line to {}", self.path.display())
            })?;
        Ok(())
    }
}

impl AppraisalStorePort for JsonlAppraisalStore {
    fn record(&self, record: AppraisalRecord) {
        if let Err(err) = self.append_raw(&record) {
            tracing::warn!(
                ?err,
                "failed to persist appraisal record; dropping silently"
            );
        }
        self.maybe_truncate();
    }

    fn aggregate(
        &self,
        agent_id: &AgentId,
        signature: &RequirementSignature,
        window: AppraisalWindow,
    ) -> AggregateStats {
        let all = self.read_all_records();
        let bucketed: Vec<AppraisalRecord> = all
            .into_iter()
            .filter(|r| &r.agent_id == agent_id && &r.requirement_signature == signature)
            .collect();
        let windowed = apply_window(bucketed, window);
        AggregateStats::from_records(&windowed)
    }
}

fn apply_window(records: Vec<AppraisalRecord>, window: AppraisalWindow) -> Vec<AppraisalRecord> {
    match window {
        AppraisalWindow::All => records,
        AppraisalWindow::LastN(n) => {
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            if records.len() <= n {
                records
            } else {
                records[records.len() - n..].to_vec()
            }
        }
        AppraisalWindow::LastHours(hours) => {
            let cutoff: DateTime<Utc> = Utc::now() - Duration::hours(i64::from(hours));
            records
                .into_iter()
                .filter(|r| r.timestamp >= cutoff)
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use sentinel_domain::agent_routing::AppraisalOutcome;
    use tempfile::TempDir;

    fn agent(s: &str) -> AgentId {
        AgentId::new(s).unwrap()
    }

    fn sig(s: &str) -> RequirementSignature {
        serde_json::from_str(&format!("\"{s}\"")).unwrap()
    }

    fn record_with(
        agent: &AgentId,
        signature: &RequirementSignature,
        outcome: AppraisalOutcome,
        cost: f32,
    ) -> AppraisalRecord {
        AppraisalRecord {
            agent_id: agent.clone(),
            requirement_signature: signature.clone(),
            outcome,
            auditor_signal: None,
            actual_cost_usd: cost,
            actual_latency_ms: 5000,
            tokens_in: 1000,
            tokens_out: 200,
            timestamp: Utc::now(),
        }
    }

    fn store(dir: &TempDir) -> JsonlAppraisalStore {
        JsonlAppraisalStore::at_path(dir.path().join("records.jsonl"))
    }

    // ---- Empty path semantics ----

    #[test]
    fn missing_file_returns_empty_aggregate() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let agg = s.aggregate(
            &agent("kimi"),
            &sig("deadbeef00000000"),
            AppraisalWindow::All,
        );
        assert!(!agg.has_data());
    }

    // ---- Record-then-read ----

    #[test]
    fn record_then_aggregate_returns_correct_counts() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.02));
        s.record(record_with(&a, &g, AppraisalOutcome::Failure, 0.015));
        let agg = s.aggregate(&a, &g, AppraisalWindow::All);
        assert_eq!(agg.cohort_size, 3);
        assert!((agg.success_rate - 2.0 / 3.0).abs() < 1e-5);
    }

    #[test]
    fn record_creates_parent_dir() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("deep").join("nest").join("records.jsonl");
        let s = JsonlAppraisalStore::at_path(nested.clone());
        s.record(record_with(
            &agent("kimi"),
            &sig("aaaaaaaaaaaaaaaa"),
            AppraisalOutcome::Success,
            0.01,
        ));
        assert!(
            nested.exists(),
            "store should auto-create parent directories"
        );
    }

    // ---- Segregation ----

    #[test]
    fn records_segregate_by_agent_id() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let g = sig("aaaaaaaaaaaaaaaa");
        s.record(record_with(
            &agent("kimi"),
            &g,
            AppraisalOutcome::Success,
            0.01,
        ));
        s.record(record_with(
            &agent("opus"),
            &g,
            AppraisalOutcome::Failure,
            0.10,
        ));
        let k = s.aggregate(&agent("kimi"), &g, AppraisalWindow::All);
        let o = s.aggregate(&agent("opus"), &g, AppraisalWindow::All);
        assert!((k.success_rate - 1.0).abs() < 1e-5);
        assert!((o.success_rate - 0.0).abs() < 1e-5);
    }

    #[test]
    fn records_segregate_by_signature() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let a = agent("kimi");
        let g1 = sig("aaaaaaaaaaaaaaaa");
        let g2 = sig("bbbbbbbbbbbbbbbb");
        s.record(record_with(&a, &g1, AppraisalOutcome::Success, 0.01));
        s.record(record_with(&a, &g2, AppraisalOutcome::Failure, 0.02));
        let v1 = s.aggregate(&a, &g1, AppraisalWindow::All);
        let v2 = s.aggregate(&a, &g2, AppraisalWindow::All);
        assert!((v1.success_rate - 1.0).abs() < 1e-5);
        assert!((v2.success_rate - 0.0).abs() < 1e-5);
    }

    // ---- Window filters ----

    #[test]
    fn last_n_window_keeps_only_recent() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        for _ in 0..5 {
            s.record(record_with(&a, &g, AppraisalOutcome::Failure, 0.01));
        }
        for _ in 0..2 {
            s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        }
        let last2 = s.aggregate(&a, &g, AppraisalWindow::LastN(2));
        assert_eq!(last2.cohort_size, 2);
        assert!((last2.success_rate - 1.0).abs() < 1e-5);
    }

    #[test]
    fn last_hours_window_excludes_old_records() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir);
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        let mut old = record_with(&a, &g, AppraisalOutcome::Failure, 0.01);
        old.timestamp = Utc::now() - ChronoDuration::hours(48);
        let recent = record_with(&a, &g, AppraisalOutcome::Success, 0.01);
        s.record(old);
        s.record(recent);
        let agg = s.aggregate(&a, &g, AppraisalWindow::LastHours(24));
        assert_eq!(agg.cohort_size, 1);
        assert!((agg.success_rate - 1.0).abs() < 1e-5);
    }

    // ---- Persistence + cross-instance read ----

    #[test]
    fn second_store_instance_sees_persisted_records() {
        let dir = TempDir::new().unwrap();
        let g = sig("aaaaaaaaaaaaaaaa");
        let a = agent("kimi");
        {
            let s1 = store(&dir);
            s1.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
            s1.record(record_with(&a, &g, AppraisalOutcome::Failure, 0.02));
        }
        // Fresh instance pointing at the same file:
        let s2 = JsonlAppraisalStore::at_path(dir.path().join("records.jsonl"));
        let agg = s2.aggregate(&a, &g, AppraisalWindow::All);
        assert_eq!(agg.cohort_size, 2, "records persisted across instances");
    }

    // ---- Malformed line tolerance ----

    #[test]
    fn malformed_lines_are_skipped_not_propagated() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("records.jsonl");
        let s = JsonlAppraisalStore::at_path(path.clone());
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        // Sneak a malformed line in (simulating partial write).
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"this is not json\n")
            .unwrap();
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.02));
        let agg = s.aggregate(&a, &g, AppraisalWindow::All);
        assert_eq!(
            agg.cohort_size, 2,
            "valid records still counted despite corrupt line"
        );
    }

    // ---- Auto-truncation ----

    #[test]
    fn truncate_keeps_trailing_half_when_oversized() {
        // Build a store with a tiny "max" by writing past the real
        // 8MB threshold isn't practical; instead test the truncate
        // path directly by stuffing the file with many records and
        // forcing the call.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("records.jsonl");
        let s = JsonlAppraisalStore::at_path(path.clone());
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        for _ in 0..100 {
            s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        }
        // Manually pad the file past MAX_LOG_SIZE to trigger truncate
        // on the next record().
        let padding = "x".repeat(usize::try_from(MAX_LOG_SIZE).unwrap_or(usize::MAX) + 1024);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(padding.as_bytes())
            .unwrap();
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        let metadata = std::fs::metadata(&path).unwrap();
        assert!(
            metadata.len() < MAX_LOG_SIZE,
            "file size should drop below MAX_LOG_SIZE after truncate, got {}",
            metadata.len()
        );
        // Verify some records still readable post-truncate.
        let agg = s.aggregate(&a, &g, AppraisalWindow::All);
        assert!(
            agg.cohort_size > 0,
            "post-truncate aggregate should still have records"
        );
    }

    // ---- Path semantics ----

    #[test]
    fn with_default_path_resolves_under_claude_dir() {
        let s = JsonlAppraisalStore::with_default_path().unwrap();
        let p = s.path().display().to_string();
        assert!(
            p.contains(".claude") && p.contains("sentinel") && p.ends_with("records.jsonl"),
            "default path should live under .claude/sentinel/...records.jsonl, got {p}"
        );
    }

    // ---- Send + Sync ----

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<JsonlAppraisalStore>();
    }
}
