//! Request shape limits — content size + rate limiting (M4.6, ContextForge pattern).
//!
//! Two failure modes that production MCP servers consistently get bitten by:
//!
//! 1. **Oversized payloads.** A malicious or buggy caller submits a 50MB
//!    tool-result body that explodes downstream — judge token costs, log
//!    lines that exceed editor open limits, OOM in the classifier.
//!    Defense: cap evidence and artifact bytes at well-defined ceilings,
//!    reject early before the cost is incurred.
//!
//! 2. **Rate flooding.** An agent stuck in a retry loop hammers a single
//!    step 200 times in 60 seconds, blowing out judge cost budgets and
//!    making the chain unreadable. Defense: per-coordinate sliding-window
//!    rate limits tracked in `SessionState`.
//!
//! # Why pure-domain
//!
//! No I/O, no clock, no async. The limits and the enforcement are both
//! pure functions of inputs. `Utc::now()` is passed in by the hook layer
//! so the policy can be tested with deterministic timestamps.
//!
//! # Where this gets called
//!
//! `step_judge::process` calls [`enforce_limits`] on the gathered evidence
//! and artifact *before* dispatching to the AI judge. If a limit fires,
//! the hook emits a StepProof with `JudgeVerdict::insufficient` and the
//! `LimitError` reason, skipping the judge invocation entirely. The denial
//! is loud (it lands in the proof chain) and cheap (no judge tokens spent).

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

use crate::evidence::Evidence;

/// Default cap on serialized evidence size, in bytes. 1 MB is generous
/// enough for normal tool-result payloads (a typical Bash output is
/// kilobytes, even chatty Linear API responses rarely exceed 100 KB)
/// and tight enough to catch the pathological 50MB-explosion cases.
pub const DEFAULT_MAX_EVIDENCE_BYTES: usize = 1_048_576;

/// Default cap on serialized artifact size, in bytes. Smaller than
/// evidence because artifacts are the typed handoff between steps —
/// they should be lean structured data, not raw payloads. 256 KB
/// is plenty for any realistic handoff (URLs, ticket IDs, structured
/// summaries) and surfaces obvious "you're shipping the wrong shape"
/// bugs early.
pub const DEFAULT_MAX_ARTIFACT_BYTES: usize = 262_144;

/// Default rate limit: 60 calls per minute per step coordinate.
/// One call per second on average is fast enough for any human-in-the-loop
/// flow and slow enough that retry storms get caught. Configurable per
/// limit policy.
pub const DEFAULT_MAX_CALLS_PER_MINUTE: usize = 60;

/// Default rate-limit window in seconds.
pub const DEFAULT_WINDOW_SECONDS: i64 = 60;

/// Per-policy configuration. Defaults are sensible; production deployments
/// override via environment / config (M4.8 brings fail-fast validation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLimits {
    /// Maximum serialized JSON size of an [`Evidence`] payload, in bytes.
    /// Larger payloads are rejected with [`LimitError::EvidenceTooLarge`].
    pub max_evidence_bytes: usize,

    /// Maximum serialized JSON size of an artifact value, in bytes.
    /// Larger values rejected with [`LimitError::ArtifactTooLarge`].
    pub max_artifact_bytes: usize,

    /// Maximum number of step invocations within the sliding window.
    /// 0 disables rate limiting entirely.
    pub max_calls_per_window: usize,

    /// Sliding window size in seconds. 60 = "calls per minute" semantics.
    pub window_seconds: i64,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_evidence_bytes: DEFAULT_MAX_EVIDENCE_BYTES,
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_calls_per_window: DEFAULT_MAX_CALLS_PER_MINUTE,
            window_seconds: DEFAULT_WINDOW_SECONDS,
        }
    }
}

/// Reasons a request was rejected. Each variant carries the relevant
/// numbers so the hook can surface a precise denial message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LimitError {
    /// Evidence payload exceeded the configured byte cap. Tuple = (actual, limit).
    EvidenceTooLarge { actual: usize, limit: usize },
    /// Artifact payload exceeded the configured byte cap.
    ArtifactTooLarge { actual: usize, limit: usize },
    /// More invocations within the sliding window than allowed.
    /// `recent_calls` = count in the window; `limit` = configured cap.
    RateLimitExceeded {
        recent_calls: usize,
        limit: usize,
        window_seconds: i64,
    },
}

impl std::fmt::Display for LimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EvidenceTooLarge { actual, limit } => write!(
                f,
                "evidence payload {actual} bytes exceeds limit of {limit} bytes",
            ),
            Self::ArtifactTooLarge { actual, limit } => write!(
                f,
                "artifact payload {actual} bytes exceeds limit of {limit} bytes",
            ),
            Self::RateLimitExceeded {
                recent_calls,
                limit,
                window_seconds,
            } => write!(
                f,
                "{recent_calls} calls in last {window_seconds}s exceeds limit of {limit}",
            ),
        }
    }
}

impl std::error::Error for LimitError {}

/// Sliding-window call history for a single (skill, phase, step) coordinate.
/// The hook layer owns one of these per coordinate via `SessionState`;
/// this module owns the eviction + check logic so policy stays pure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallWindow {
    /// Timestamps of recent calls, ordered oldest-first. Eviction
    /// happens lazily on each [`record_call`] / [`is_over_limit`] call —
    /// no background thread needed.
    timestamps: VecDeque<DateTime<Utc>>,
}

impl CallWindow {
    /// Build an empty window.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop timestamps that fall outside the sliding window. Must be
    /// called before checking length — otherwise stale entries
    /// cause false-positive rate-limit denials.
    pub fn evict_expired(&mut self, now: DateTime<Utc>, window_seconds: i64) {
        let cutoff = now - Duration::seconds(window_seconds);
        while let Some(&front) = self.timestamps.front() {
            if front < cutoff {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Record a new call timestamp at the back of the window. Eviction
    /// of stale entries happens first so the window stays bounded.
    pub fn record_call(&mut self, now: DateTime<Utc>, window_seconds: i64) {
        self.evict_expired(now, window_seconds);
        self.timestamps.push_back(now);
    }

    /// Count of calls currently within the window. Calls
    /// [`evict_expired`] first.
    pub fn current_count(&mut self, now: DateTime<Utc>, window_seconds: i64) -> usize {
        self.evict_expired(now, window_seconds);
        self.timestamps.len()
    }

    /// True when adding *one more* call would exceed the limit. Used
    /// to reject *before* recording so the chain doesn't get a denial
    /// proof and a successful proof for the same call.
    ///
    /// `limit == 0` disables the check (returns false always).
    pub fn would_exceed(&mut self, now: DateTime<Utc>, window_seconds: i64, limit: usize) -> bool {
        if limit == 0 {
            return false;
        }
        self.current_count(now, window_seconds) >= limit
    }
}

/// Enforce all three limits against a candidate request.
///
/// **Pure function.** Caller passes in the evidence/artifact, the
/// current `now`, and a mutable reference to the per-coordinate
/// `CallWindow`. Returns `Ok(())` on accept, `Err(LimitError)` on the
/// first failing check.
///
/// Order: size checks first (cheap, no state mutation), then the rate
/// check (mutates the window only if all checks pass — see `record_on_accept`).
/// We deliberately do NOT record the call on rejection: rejecting a
/// request *because of* a flood and then counting that rejection
/// toward the next flood is the rate-limit equivalent of compounding
/// debt. Reject silently, count successes only.
pub fn enforce_limits(
    evidence: &Evidence,
    artifact: &serde_json::Value,
    limits: &RequestLimits,
    now: DateTime<Utc>,
    window: &mut CallWindow,
    record_on_accept: bool,
) -> Result<(), LimitError> {
    // 1. Evidence size — serializing once (the hash path also serializes,
    //    but caching the bytes here would couple to step_proof internals).
    let evidence_bytes = serde_json::to_vec(evidence)
        .expect("Evidence serialization is infallible — see step_proof.rs comment");
    if evidence_bytes.len() > limits.max_evidence_bytes {
        return Err(LimitError::EvidenceTooLarge {
            actual: evidence_bytes.len(),
            limit: limits.max_evidence_bytes,
        });
    }

    // 2. Artifact size — Null artifacts serialize to 4 bytes ("null"),
    //    which is below any sensible limit. We still serialize and
    //    measure for explicit symmetry with evidence.
    let artifact_bytes = serde_json::to_vec(artifact)
        .expect("Value serialization is infallible — Value covers all serializable shapes");
    if artifact_bytes.len() > limits.max_artifact_bytes {
        return Err(LimitError::ArtifactTooLarge {
            actual: artifact_bytes.len(),
            limit: limits.max_artifact_bytes,
        });
    }

    // 3. Rate check — would_exceed evicts stale entries first, so the
    //    count it sees is only timestamps within the window.
    if window.would_exceed(now, limits.window_seconds, limits.max_calls_per_window) {
        return Err(LimitError::RateLimitExceeded {
            recent_calls: window.current_count(now, limits.window_seconds),
            limit: limits.max_calls_per_window,
            window_seconds: limits.window_seconds,
        });
    }

    // All checks passed — optionally record the call so the next
    // invocation sees it. record_on_accept is a knob the caller
    // controls because some hook flows want to enforce twice (gate
    // pre-judge AND audit post-judge) without double-counting.
    if record_on_accept {
        window.record_call(now, limits.window_seconds);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn small_evidence() -> Evidence {
        // Default Evidence is empty — serializes to a few bytes.
        Evidence::default()
    }

    fn large_evidence(bytes: usize) -> Evidence {
        // Pad the custom field with a string of the given size. Every byte
        // of the string contributes ~1 byte to the JSON serialization
        // (printable ASCII, no escaping). A quoted string adds 2 bytes
        // for the quotes — negligible at the sizes we care about.
        let mut e = Evidence::default();
        e.custom = serde_json::Value::String("x".repeat(bytes));
        e
    }

    // ─── RequestLimits defaults ──────────────────────────────────────

    #[test]
    fn defaults_are_documented_constants() {
        let l = RequestLimits::default();
        assert_eq!(l.max_evidence_bytes, DEFAULT_MAX_EVIDENCE_BYTES);
        assert_eq!(l.max_artifact_bytes, DEFAULT_MAX_ARTIFACT_BYTES);
        assert_eq!(l.max_calls_per_window, DEFAULT_MAX_CALLS_PER_MINUTE);
        assert_eq!(l.window_seconds, DEFAULT_WINDOW_SECONDS);
    }

    #[test]
    fn limits_round_trip_through_serde() {
        let limits = RequestLimits {
            max_evidence_bytes: 100,
            max_artifact_bytes: 50,
            max_calls_per_window: 5,
            window_seconds: 10,
        };
        let json = serde_json::to_string(&limits).unwrap();
        let restored: RequestLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, limits);
    }

    // ─── CallWindow ──────────────────────────────────────────────────

    #[test]
    fn empty_window_has_zero_count() {
        let mut w = CallWindow::new();
        assert_eq!(w.current_count(ts(0), 60), 0);
    }

    #[test]
    fn record_call_increments_count() {
        let mut w = CallWindow::new();
        w.record_call(ts(10), 60);
        w.record_call(ts(20), 60);
        assert_eq!(w.current_count(ts(30), 60), 2);
    }

    #[test]
    fn evict_expired_drops_old_timestamps() {
        // Window = 60s, now = 100s, two calls at 10s and 70s.
        // 10s is outside the window (cutoff = 40s); 70s stays.
        let mut w = CallWindow::new();
        w.record_call(ts(10), 60);
        w.record_call(ts(70), 60);
        assert_eq!(w.current_count(ts(100), 60), 1);
    }

    #[test]
    fn evict_handles_window_change_at_query_time() {
        // Eviction reads `window_seconds` at call time — operators
        // tightening the limit mid-session immediately see the
        // effect. Two calls at 0s and 30s; query at 60s with window=15s
        // evicts both.
        let mut w = CallWindow::new();
        w.record_call(ts(0), 60);
        w.record_call(ts(30), 60);
        assert_eq!(w.current_count(ts(60), 15), 0);
    }

    #[test]
    fn would_exceed_returns_false_when_under_limit() {
        let mut w = CallWindow::new();
        w.record_call(ts(0), 60);
        assert!(!w.would_exceed(ts(0), 60, 2));
    }

    #[test]
    fn would_exceed_returns_true_at_limit() {
        // Limit = 2, two calls already in window — third would exceed.
        let mut w = CallWindow::new();
        w.record_call(ts(0), 60);
        w.record_call(ts(10), 60);
        assert!(w.would_exceed(ts(20), 60, 2));
    }

    #[test]
    fn would_exceed_disabled_when_limit_zero() {
        // limit = 0 turns off rate limiting. Useful for trusted
        // background coordinates (e.g. session_init) that the
        // operator wants exempt.
        let mut w = CallWindow::new();
        for s in 0..1000 {
            w.record_call(ts(s), 60);
        }
        assert!(!w.would_exceed(ts(1001), 60, 0));
    }

    // ─── enforce_limits ──────────────────────────────────────────────

    #[test]
    fn enforce_passes_clean_request_with_default_limits() {
        let mut w = CallWindow::new();
        let result = enforce_limits(
            &small_evidence(),
            &serde_json::Value::Null,
            &RequestLimits::default(),
            ts(0),
            &mut w,
            true,
        );
        assert!(result.is_ok());
        assert_eq!(w.current_count(ts(0), 60), 1);
    }

    #[test]
    fn enforce_rejects_evidence_over_limit() {
        // 200-byte evidence, 100-byte limit.
        let mut w = CallWindow::new();
        let limits = RequestLimits {
            max_evidence_bytes: 100,
            ..RequestLimits::default()
        };
        let result = enforce_limits(
            &large_evidence(200),
            &serde_json::Value::Null,
            &limits,
            ts(0),
            &mut w,
            true,
        );
        match result {
            Err(LimitError::EvidenceTooLarge { actual, limit }) => {
                assert!(actual > 100, "actual {actual} should exceed 100");
                assert_eq!(limit, 100);
            }
            other => panic!("expected EvidenceTooLarge, got {other:?}"),
        }
        // Rejected → not counted toward the rate window.
        assert_eq!(w.current_count(ts(0), 60), 0);
    }

    #[test]
    fn enforce_rejects_artifact_over_limit() {
        let mut w = CallWindow::new();
        let limits = RequestLimits {
            max_artifact_bytes: 50,
            ..RequestLimits::default()
        };
        let big = serde_json::Value::String("y".repeat(100));
        let result = enforce_limits(&small_evidence(), &big, &limits, ts(0), &mut w, true);
        match result {
            Err(LimitError::ArtifactTooLarge { actual, limit }) => {
                assert!(actual > 50);
                assert_eq!(limit, 50);
            }
            other => panic!("expected ArtifactTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn enforce_rejects_when_rate_window_full() {
        // Window of 2 calls already used; third must be rejected.
        let mut w = CallWindow::new();
        let limits = RequestLimits {
            max_calls_per_window: 2,
            window_seconds: 60,
            ..RequestLimits::default()
        };
        // Pre-load the window with two calls. Note we pass
        // record_on_accept=true so each successful call is counted.
        for _ in 0..2 {
            enforce_limits(
                &small_evidence(),
                &serde_json::Value::Null,
                &limits,
                ts(0),
                &mut w,
                true,
            )
            .unwrap();
        }
        // Third call within the same window must fail.
        let result = enforce_limits(
            &small_evidence(),
            &serde_json::Value::Null,
            &limits,
            ts(0),
            &mut w,
            true,
        );
        match result {
            Err(LimitError::RateLimitExceeded {
                recent_calls,
                limit,
                window_seconds,
            }) => {
                assert_eq!(recent_calls, 2);
                assert_eq!(limit, 2);
                assert_eq!(window_seconds, 60);
            }
            other => panic!("expected RateLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn enforce_does_not_count_rejected_calls_against_window() {
        // Critical invariant: a flood that gets rate-limited must NOT
        // make the next caller's situation worse. Otherwise the limit
        // becomes self-amplifying — the more you get rejected, the
        // longer until you can call again. Reject silently, count
        // successes only.
        let mut w = CallWindow::new();
        let limits = RequestLimits {
            max_calls_per_window: 1,
            window_seconds: 60,
            ..RequestLimits::default()
        };
        // First call accepted.
        enforce_limits(
            &small_evidence(),
            &serde_json::Value::Null,
            &limits,
            ts(0),
            &mut w,
            true,
        )
        .unwrap();
        // 50 rejected calls.
        for _ in 0..50 {
            let _ = enforce_limits(
                &small_evidence(),
                &serde_json::Value::Null,
                &limits,
                ts(0),
                &mut w,
                true,
            );
        }
        // After window expires, count should be 0 (only one call was
        // ever recorded — the first; 50 rejections didn't add to it).
        assert_eq!(w.current_count(ts(61), 60), 0);
    }

    #[test]
    fn enforce_skips_recording_when_record_on_accept_false() {
        // The two-phase enforce-then-record knob lets gate hooks
        // pre-check without consuming the budget — the actual
        // step_judge call records on accept.
        let mut w = CallWindow::new();
        enforce_limits(
            &small_evidence(),
            &serde_json::Value::Null,
            &RequestLimits::default(),
            ts(0),
            &mut w,
            false,
        )
        .unwrap();
        assert_eq!(w.current_count(ts(0), 60), 0);
    }

    #[test]
    fn enforce_checks_evidence_before_rate_limit() {
        // Order matters for diagnostic clarity: "your evidence is
        // too big" is more actionable than "you're being rate-limited"
        // when both apply. Evidence is checked first.
        let mut w = CallWindow::new();
        let limits = RequestLimits {
            max_evidence_bytes: 10,
            max_calls_per_window: 0, // would also fail rate (limit=0 disables rate)
            ..RequestLimits::default()
        };
        // Rate is disabled (limit=0), so the failure must come from evidence.
        let result = enforce_limits(
            &large_evidence(100),
            &serde_json::Value::Null,
            &limits,
            ts(0),
            &mut w,
            true,
        );
        assert!(matches!(result, Err(LimitError::EvidenceTooLarge { .. })));
    }

    #[test]
    fn limit_error_display_includes_relevant_numbers() {
        // Hook deny messages surface the Display impl. Pin that the
        // numbers actually appear so operators get actionable text.
        let e = LimitError::EvidenceTooLarge {
            actual: 1500,
            limit: 1000,
        };
        let s = format!("{e}");
        assert!(s.contains("1500"));
        assert!(s.contains("1000"));

        let r = LimitError::RateLimitExceeded {
            recent_calls: 100,
            limit: 60,
            window_seconds: 60,
        };
        let s = format!("{r}");
        assert!(s.contains("100"));
        assert!(s.contains("60"));
    }
}
