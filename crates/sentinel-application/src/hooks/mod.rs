//! Hook Implementations
//!
//! All hooks run through the sentinel Rust engine.
//! Each module implements one hook. Add new hooks here and
//! update HOOK_NAMES to keep the count accurate.

pub mod account_cascade;
pub mod activity_tracker;
mod block_context;
pub mod commit_hygiene;
pub mod commit_message_validator;
pub mod context_monitor;
pub mod cwd_changed;
pub mod doc_cleanup;
pub mod doc_drift;
pub mod error_reporter;
pub mod evidence_collector;
pub mod execution_log;
pub mod git_hygiene;
pub mod hygiene_override;
pub mod mcp_health;
pub mod memory_extract;
pub mod memory_inject;
pub mod permission_denied;
pub mod phase_gate;
pub mod phase_validator;
pub mod plan_organizer;
pub mod post_compact;
pub mod pre_commit_verification;
pub mod pre_compact;
pub mod pre_push_steel_test;
pub mod session_end;
pub mod session_init;
pub mod setup;
pub mod skill_router;
pub mod skill_telemetry;
pub mod stop_failure;
pub mod subagent_start;
pub mod subagent_stop;
pub mod task_completed;
pub mod task_created;
pub mod task_persist;
pub mod task_rehydrate;
pub mod teammate_idle;
pub mod todo_interceptor;
pub mod todo_loader;
pub mod verification_gate;
pub mod wrangler_guard;

/// All hook module names — used for dynamic counting.
/// Keep in sync with the `pub mod` declarations above.
pub const HOOK_NAMES: &[&str] = &[
    "account_cascade",
    "activity_tracker",
    "commit_hygiene",
    "commit_message_validator",
    "context_monitor",
    "cwd_changed",
    "doc_cleanup",
    "doc_drift",
    "error_reporter",
    "evidence_collector",
    "execution_log",
    "git_hygiene",
    "hygiene_override",
    "mcp_health",
    "memory_extract",
    "memory_inject",
    "permission_denied",
    "phase_gate",
    "phase_validator",
    "plan_organizer",
    "post_compact",
    "pre_commit_verification",
    "pre_compact",
    "pre_push_steel_test",
    "session_end",
    "session_init",
    "setup",
    "skill_router",
    "skill_telemetry",
    "stop_failure",
    "subagent_start",
    "subagent_stop",
    "task_completed",
    "task_created",
    "task_persist",
    "task_rehydrate",
    "teammate_idle",
    "todo_interceptor",
    "todo_loader",
    "verification_gate",
    "wrangler_guard",
];

/// Port for git status queries — implemented by the infrastructure layer.
/// The git-dependent hooks accept this trait so the application layer
/// stays decoupled from infrastructure (no cyclic dependency).
pub trait GitStatusPort {
    /// Check if there are uncommitted changes in the given repo path.
    fn has_uncommitted_changes(&self, repo_path: &str) -> anyhow::Result<bool>;

    /// Get list of changed files (staged + unstaged).
    fn changed_files(&self, repo_path: &str) -> anyhow::Result<Vec<String>>;
}
