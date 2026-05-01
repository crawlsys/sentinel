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

// ---------------------------------------------------------------------------
// Async/runtime timeouts and freshness windows
// ---------------------------------------------------------------------------

/// Hard wall-clock timeout for any async hook work. No Qdrant/API/MCP call may
/// block a hook longer than this. Used by `hooks::run_async`.
pub const RUN_ASYNC_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the marketplace dependency-check cache (`dep_check.rs`) is
/// considered fresh before it is rebuilt. One day is long enough that the
/// check rarely re-runs in normal use, short enough that adding a new
/// crate-level dep gets noticed within a working day.
pub const DEP_CHECK_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How recent a `plans/*.md` file must be to satisfy the resumed-session
/// fallback in `tool_usage_gate`. One week balances "session resumed days
/// later" against "stale plan from a different feature still on disk".
pub const PLAN_FILE_FRESH_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How old a session-events directory must be before
/// `channel_events::cleanup_stale_sessions` removes it during SessionStart.
/// One day matches `DEP_CHECK_CACHE_TTL` numerically but the meanings are
/// independent — kept distinct so they can drift apart later.
pub const STALE_SESSION_EVENTS_AGE: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------------------------------
// Human-team baseline (SEN-15 ROI)
// ---------------------------------------------------------------------------

/// Fully-loaded mid-level engineer cost per year (USD). Includes salary,
/// benefits, taxes, equipment, office overhead. $170k is the conservative
/// US-based mid-level rate at writing.
pub const HUMAN_FULLY_LOADED_USD_PER_YEAR: f64 = 170_000.0;

/// Fully-loaded engineer cost per working day (USD). Derived as
/// `HUMAN_FULLY_LOADED_USD_PER_YEAR / 260` (52 weeks * 5 working days).
/// Stored as a constant rather than computed so the ROI rollup is
/// deterministic and reviewers don't have to retrace the arithmetic.
pub const HUMAN_FULLY_LOADED_USD_PER_DAY: f64 = 654.0;

/// Average mid-level developer velocity expressed as days per story
/// point. Derived from "10 points per week = 0.5 days per point" — a
/// commonly cited team-velocity benchmark. Override per project if your
/// team's actual velocity differs materially.
pub const HUMAN_DAYS_PER_POINT: f64 = 0.5;

/// Fully-loaded human cost per story point (USD). Equal to
/// `HUMAN_FULLY_LOADED_USD_PER_DAY * HUMAN_DAYS_PER_POINT` ($654 * 0.5 = $327).
/// This is the headline divisor in the ROI ratio.
pub const HUMAN_USD_PER_POINT: f64 = 327.0;

/// Working days per year used to project annual savings from a per-day
/// or per-point delta. 260 = 52 * 5.
pub const HUMAN_WORKING_DAYS_PER_YEAR: f64 = 260.0;
