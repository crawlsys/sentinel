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

#![allow(clippy::missing_const_for_fn)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use consul_domain::identity::SessionId;

/// Default TTL for cached approvals. Long enough for the operator
/// to switch context back to Claude Code and retry the action,
/// short enough that a forgotten approval can't authorize a much-
/// later retry.
pub const DEFAULT_TTL: Duration = Duration::from_mins(5);

#[derive(Debug, Clone)]
struct ApprovalEntry {
    recorded_at: Instant,
    /// Audit-log breadcrumb: the operator's spoken transcript that
    /// produced this approval. Surfaced in the hook's allow
    /// message so the audit trail shows what was approved.
    transcript: String,
}

/// Cheaply cloneable approval cache. Internally `Arc<Mutex<_>>`.
#[derive(Clone, Debug, Default)]
pub struct CatastrophicApprovalCache {
    inner: Arc<Mutex<HashMap<(SessionId, String), ApprovalEntry>>>,
    ttl: Duration,
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
        }
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
                recorded_at: Instant::now(),
                transcript,
            },
        );
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
        Some(ConsumedApproval {
            transcript: entry.transcript,
            age: entry.recorded_at.elapsed(),
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
        let now = Instant::now();
        let ttl = self.ttl;
        g.retain(|_, entry| now.duration_since(entry.recorded_at) < ttl);
    }
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
}
