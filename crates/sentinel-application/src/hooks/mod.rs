//! Hook Implementations
//!
//! All hooks run through the sentinel Rust engine.
//! Each module implements one hook. Add new hooks here and
//! update HOOK_NAMES to keep the count accurate.

pub mod commit_hygiene;
pub mod context_monitor;
pub mod doc_cleanup;
pub mod doc_drift;
pub mod error_reporter;
pub mod evidence_collector;
pub mod execution_log;
pub mod git_hygiene;
pub mod hygiene_override;
pub mod mcp_health;
pub mod phase_gate;
pub mod phase_validator;
pub mod pre_commit_verification;
pub mod pre_push_steel_test;
pub mod session_init;
pub mod skill_router;
pub mod skill_telemetry;
pub mod todo_interceptor;
pub mod todo_loader;
pub mod verification_gate;

/// All hook module names — used for dynamic counting.
/// Keep in sync with the `pub mod` declarations above.
pub const HOOK_NAMES: &[&str] = &[
    "commit_hygiene",
    "context_monitor",
    "doc_cleanup",
    "doc_drift",
    "error_reporter",
    "evidence_collector",
    "execution_log",
    "git_hygiene",
    "hygiene_override",
    "mcp_health",
    "phase_gate",
    "phase_validator",
    "pre_commit_verification",
    "pre_push_steel_test",
    "session_init",
    "skill_router",
    "skill_telemetry",
    "todo_interceptor",
    "todo_loader",
    "verification_gate",
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
