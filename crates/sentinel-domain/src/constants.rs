//! Domain constants — named values for timeouts, limits, and thresholds.
//!
//! These are domain policy values. Infrastructure may read them, but only
//! the domain layer defines them. Hooks import these instead of using
//! magic numbers inline.

use std::time::Duration;

// ---------------------------------------------------------------------------
// HTTP / vector-store timeouts
// ---------------------------------------------------------------------------

/// Quick vector search (semantic lookup, single query).
pub const VECTOR_QUERY_TIMEOUT: Duration = Duration::from_millis(800);

/// Batch vector upsert or multi-step vector operation.
pub const VECTOR_BATCH_TIMEOUT: Duration = Duration::from_millis(1500);

/// Standard external API call (Qdrant scroll, `set_payload`).
/// Capped by `run_async`'s 3s wall-clock timeout.
pub const API_CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Long-running external API call (bulk upsert, full index).
/// Capped by `run_async`'s 3s wall-clock timeout.
pub const API_CALL_TIMEOUT_LONG: Duration = Duration::from_secs(3);

/// Memory feedback / light verification call.
pub const API_CALL_TIMEOUT_SHORT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Hook cooldown periods
// ---------------------------------------------------------------------------

/// How often `context_monitor` / `error_reporter` may fire (10 min).
pub const HOOK_COOLDOWN_SHORT_MS: u64 = 10 * 60 * 1000;

/// How often `commit_hygiene` may fire (15 min).
pub const HOOK_COOLDOWN_MEDIUM_MS: u64 = 15 * 60 * 1000;

/// How often `activity_tracker` may fire (20 min).
pub const HOOK_COOLDOWN_LONG_MS: u64 = 20 * 60 * 1000;

/// How often `doc_drift` / `doc_cleanup` may fire (30 min).
pub const HOOK_COOLDOWN_DOC_MS: u64 = 30 * 60 * 1000;

/// Verification gate cooldown (5 min).
pub const HOOK_COOLDOWN_VERIFY_MS: u64 = 5 * 60 * 1000;

// ---------------------------------------------------------------------------
// Steel / test validity
// ---------------------------------------------------------------------------

/// How long a passing Steel browser test remains valid (2 hours).
pub const STEEL_TEST_VALIDITY: Duration = Duration::from_secs(2 * 60 * 60);

// ---------------------------------------------------------------------------
// Memory system
// ---------------------------------------------------------------------------

/// Maximum age for precomputed search results before refresh (5 min).
pub const PRECOMPUTED_SEARCH_MAX_AGE_SECS: i64 = 300;

/// Tool calls between re-index triggers.
pub const REINDEX_TOOL_CALL_THRESHOLD: u64 = 50;

/// Minimum exchange length for memory extraction (chars).
pub const MIN_EXCHANGE_LENGTH: usize = 100;

/// Maximum memories to verify per session.
pub const MAX_VERIFY_PER_SESSION: usize = 10;

/// Days before a memory is considered stale for verification.
pub const VERIFY_STALE_DAYS: i64 = 7;

/// Dedup context cap (bytes) for memory injection.
pub const DEDUP_CONTEXT_CAP: usize = 50 * 1024;

/// Overlap threshold for dedup (0.0-1.0).
pub const DEDUP_OVERLAP_THRESHOLD: f64 = 0.60;

/// Minimum chunk size for session indexing (chars).
pub const MIN_CHUNK_CHARS: usize = 50;

// ---------------------------------------------------------------------------
// Proof engine
// ---------------------------------------------------------------------------

/// Cooldown between proof resubmissions (seconds).
pub const PROOF_RESUBMIT_COOLDOWN_SECS: i64 = 30;

/// Max rapid failures before backoff.
pub const PROOF_MAX_RAPID_FAILURES: u32 = 3;

// ---------------------------------------------------------------------------
// Git / hygiene
// ---------------------------------------------------------------------------

/// Max uncommitted files before `git_hygiene` warns.
pub const MAX_UNCOMMITTED_FILES: usize = 10;

/// Hygiene override token TTL (seconds).
pub const OVERRIDE_TTL_SECS: u64 = 60;

/// Min changed files for `commit_hygiene` to trigger.
pub const COMMIT_HYGIENE_MIN_FILES: usize = 3;

/// Min hook calls before `activity_tracker` summarizes.
pub const ACTIVITY_TRACKER_MIN_CALLS: usize = 15;

/// Max errors included in context injection.
pub const MAX_ERRORS_IN_CONTEXT: usize = 3;

/// Min skill directories expected (sanity check).
pub const MIN_SKILL_DIRS: usize = 40;
