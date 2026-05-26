//! A2 — In-memory appraisal store (test helper + reference impl).
//!
//! Per `docs/a2-capability-aware-routing.md` §4. The production store
//! (JSONL-backed, Phase 3b in `sentinel-infrastructure`) persists
//! records across sessions; this in-memory version is suitable for
//! unit tests of the router + hooks that only need single-session
//! state.
//!
//! **R5 quarantine boundary**: appraisal records flow ONE WAY from
//! the dispatching hook → store → router as a tie-breaker input.
//! Agents must never see appraisal data as feedback. This applies
//! to both the in-memory and JSONL adapters — the contract is on
//! the port, not the implementation.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use chrono::Utc;
use sentinel_domain::agent_routing::{
    AggregateStats, AppraisalRecord, AppraisalWindow, RequirementSignature,
};
use sentinel_domain::capability::AgentId;
use sentinel_domain::ports::AppraisalStorePort;

/// In-memory [`AppraisalStorePort`] backed by a `HashMap` keyed by
/// `(agent_id, requirement_signature)`. Suitable for tests and for
/// single-session callers that don't need cross-session persistence.
///
/// Interior mutability uses [`RwLock`] so the store can be wrapped in
/// `Arc<dyn AppraisalStorePort>` and shared across hooks in the same
/// session. `record` writes are best-effort: if the lock is poisoned
/// the call is silently dropped + a `tracing::warn` is emitted.
#[derive(Debug, Default)]
pub struct InMemoryAppraisalStore {
    records: RwLock<HashMap<(AgentId, RequirementSignature), Vec<AppraisalRecord>>>,
}

impl InMemoryAppraisalStore {
    /// Construct an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current record count for tests + observability.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.records
            .read()
            .map_or(0, |g| g.values().map(Vec::len).sum())
    }
}

impl AppraisalStorePort for InMemoryAppraisalStore {
    fn record(&self, record: AppraisalRecord) {
        let key = (
            record.agent_id.clone(),
            record.requirement_signature.clone(),
        );
        match self.records.write() {
            Ok(mut guard) => {
                guard.entry(key).or_default().push(record);
            }
            Err(poisoned) => {
                tracing::warn!(
                    "appraisal store lock poisoned ({}); record dropped",
                    poisoned.to_string()
                );
            }
        }
    }

    fn aggregate(
        &self,
        agent_id: &AgentId,
        signature: &RequirementSignature,
        window: AppraisalWindow,
    ) -> AggregateStats {
        let Ok(guard) = self.records.read() else {
            return AggregateStats::empty();
        };
        let Some(records) = guard.get(&(agent_id.clone(), signature.clone())) else {
            return AggregateStats::empty();
        };
        let filtered = apply_window(records, window);
        AggregateStats::from_records(&filtered)
    }
}

fn apply_window(records: &[AppraisalRecord], window: AppraisalWindow) -> Vec<AppraisalRecord> {
    match window {
        AppraisalWindow::All => records.to_vec(),
        AppraisalWindow::LastN(n) => {
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            if records.len() <= n {
                records.to_vec()
            } else {
                records[records.len() - n..].to_vec()
            }
        }
        AppraisalWindow::LastHours(hours) => {
            let cutoff = Utc::now() - Duration::from_secs(u64::from(hours) * 3600);
            records
                .iter()
                .filter(|r| r.timestamp >= cutoff)
                .cloned()
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

    fn agent(s: &str) -> AgentId {
        AgentId::new(s).unwrap()
    }

    fn sig(s: &str) -> RequirementSignature {
        // Test-only — use the public `of` in real code. We borrow a
        // string for fixture purposes; in production this comes from
        // hashing a real requirement.
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

    #[test]
    fn empty_store_aggregates_to_empty() {
        let s = InMemoryAppraisalStore::new();
        let agg = s.aggregate(
            &agent("kimi"),
            &sig("deadbeefdeadbeef"),
            AppraisalWindow::All,
        );
        assert!(!agg.has_data());
        assert_eq!(agg.cohort_size, 0);
    }

    #[test]
    fn record_then_aggregate_returns_correct_counts() {
        let s = InMemoryAppraisalStore::new();
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
    fn records_segregate_by_agent_id() {
        let s = InMemoryAppraisalStore::new();
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
        let s = InMemoryAppraisalStore::new();
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

    #[test]
    fn last_n_window_keeps_only_recent() {
        let s = InMemoryAppraisalStore::new();
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        // 5 failures + 2 successes = 2/7 strict success rate over All.
        for _ in 0..5 {
            s.record(record_with(&a, &g, AppraisalOutcome::Failure, 0.01));
        }
        for _ in 0..2 {
            s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        }
        let last2 = s.aggregate(&a, &g, AppraisalWindow::LastN(2));
        assert_eq!(last2.cohort_size, 2);
        assert!(
            (last2.success_rate - 1.0).abs() < 1e-5,
            "last 2 are both Success"
        );
    }

    #[test]
    fn last_hours_window_excludes_old_records() {
        let s = InMemoryAppraisalStore::new();
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        let mut old = record_with(&a, &g, AppraisalOutcome::Failure, 0.01);
        old.timestamp = Utc::now() - ChronoDuration::hours(48);
        let recent = record_with(&a, &g, AppraisalOutcome::Success, 0.01);
        s.record(old);
        s.record(recent);
        // Last 24h → only the recent record.
        let agg = s.aggregate(&a, &g, AppraisalWindow::LastHours(24));
        assert_eq!(agg.cohort_size, 1);
        assert!((agg.success_rate - 1.0).abs() < 1e-5);
    }

    #[test]
    fn record_count_reflects_persisted() {
        let s = InMemoryAppraisalStore::new();
        let a = agent("kimi");
        let g = sig("aaaaaaaaaaaaaaaa");
        assert_eq!(s.record_count(), 0);
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.01));
        s.record(record_with(&a, &g, AppraisalOutcome::Success, 0.02));
        assert_eq!(s.record_count(), 2);
    }

    #[test]
    fn store_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<InMemoryAppraisalStore>();
    }
}
