//! Hook Implementations
//!
//! Each module implements one hook from the Claude Code marketplace,
//! ported from the original Node.js hooks to Rust.

pub mod commit_hygiene;
pub mod context_monitor;
pub mod doc_cleanup;
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

/// Port for git status queries — implemented by the infrastructure layer.
/// The git-dependent hooks accept this trait so the application layer
/// stays decoupled from infrastructure (no cyclic dependency).
pub trait GitStatusPort {
    /// Check if there are uncommitted changes in the given repo path.
    fn has_uncommitted_changes(&self, repo_path: &str) -> anyhow::Result<bool>;

    /// Get list of changed files (staged + unstaged).
    fn changed_files(&self, repo_path: &str) -> anyhow::Result<Vec<String>>;
}
