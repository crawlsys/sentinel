//! Hook Implementations
//!
//! All hooks run through the sentinel Rust engine.
//! Each module implements one hook. Add new hooks here and
//! update HOOK_NAMES to keep the count accurate.

pub mod account_cascade;
pub mod activity_tracker;
mod block_context;
pub mod build_auto_monitor;
pub mod build_notify;
pub mod commit_hygiene;
pub mod commit_message_validator;
pub mod context_monitor;
pub mod cwd_changed;
pub mod db_ops_gate;
pub mod doc_cleanup;
pub mod doc_drift;
pub mod doppler_auth0_gate;
pub mod error_reporter;
pub mod evidence_collector;
pub mod execution_log;
pub mod git_hygiene;
pub mod hygiene_override;
pub mod hygiene_reminders;
pub mod linear_lifecycle;
pub mod mcp_health;
pub mod memory_extract;
pub mod memory_feedback;
pub mod memory_inject;
pub mod memory_verify;
pub mod permission_denied;
pub mod phase_gate;
pub mod phase_validator;
pub mod plan_organizer;
pub mod post_compact;
pub mod pr_auto_monitor;
pub mod pr_merge_gate;
pub mod pre_commit_verification;
pub mod pre_compact;
pub mod pre_push_steel_test;
pub mod session_end;
pub mod session_index;
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
pub mod tool_usage_gate;
pub mod verification_gate;
pub mod worktree_reminder;

// ---------------------------------------------------------------------------
// Centralized path helpers — all sentinel-owned files live under
// `~/.claude/sentinel/` to keep the user's `.claude/` directory clean.
// ---------------------------------------------------------------------------

/// Return `<home>/.claude/sentinel`.
pub fn sentinel_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".claude").join("sentinel")
}

/// Return `<home>/.claude/sentinel/metrics`.
///
/// All metric/telemetry JSONL files go here (previously `~/.claude/metrics/`).
pub fn metrics_dir(home: &std::path::Path) -> std::path::PathBuf {
    sentinel_dir(home).join("metrics")
}

/// All hook module names — used for dynamic counting.
/// Keep in sync with the `pub mod` declarations above.
pub const HOOK_NAMES: &[&str] = &[
    "account_cascade",
    "activity_tracker",
    "build_auto_monitor",
    "commit_hygiene",
    "commit_message_validator",
    "context_monitor",
    "cwd_changed",
    "db_ops_gate",
    "doc_cleanup",
    "doc_drift",
    "doppler_auth0_gate",
    "error_reporter",
    "evidence_collector",
    "execution_log",
    "git_hygiene",
    "hygiene_override",
    "hygiene_reminders",
    "linear_lifecycle",
    "mcp_health",
    "memory_extract",
    "memory_feedback",
    "memory_inject",
    "memory_verify",
    "permission_denied",
    "phase_gate",
    "phase_validator",
    "plan_organizer",
    "post_compact",
    "pr_auto_monitor",
    "pr_merge_gate",
    "pre_commit_verification",
    "pre_compact",
    "pre_push_steel_test",
    "session_end",
    "session_index",
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
    "tool_usage_gate",
    "verification_gate",
    "worktree_reminder",
];

// ---------------------------------------------------------------------------
// Shared async runtime helper
// ---------------------------------------------------------------------------

/// Hard wall-clock timeout for all async hook work.
/// No Qdrant/API call may block a hook longer than this.
const RUN_ASYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Run an async block safely with a hard wall-clock timeout.
///
/// Guarantees:
/// 1. Never panics from nested tokio runtimes (uses scoped thread).
/// 2. Never blocks longer than [`RUN_ASYNC_TIMEOUT`] — returns `T::default()`
///    if the future doesn't complete in time.
///
/// Used by all memory/Qdrant hooks that need to make async HTTP calls.
pub fn run_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send + Default,
{
    // Always run on a scoped thread with its own runtime.
    // This avoids nested-runtime panics AND lets us enforce a wall-clock timeout
    // via the thread join timeout (which kills slow DNS, TCP connect, etc).
    std::thread::scope(|s| {
        let handle = s.spawn(|| {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(async {
                    // Apply tokio timeout on top of reqwest timeouts
                    match tokio::time::timeout(RUN_ASYNC_TIMEOUT, future).await {
                        Ok(result) => result,
                        Err(_) => {
                            tracing::debug!("run_async: wall-clock timeout exceeded");
                            T::default()
                        }
                    }
                }),
                Err(_) => T::default(),
            }
        });
        // The scoped thread must join (Rust guarantees this), but the tokio
        // timeout inside ensures the future is cancelled within RUN_ASYNC_TIMEOUT.
        handle.join().unwrap_or_default()
    })
}

// ---------------------------------------------------------------------------
// Hexagonal ports — re-exported from the domain layer
// ---------------------------------------------------------------------------
// Port trait definitions live in sentinel-domain::ports (where they belong
// per hexagonal architecture). Re-exported here so that hook modules can
// continue using `use super::GitStatusPort` etc. without changes.
// Infrastructure implementations live in sentinel-infrastructure.
// The CLI (hook_cmd.rs) constructs concrete adapters and injects them.

pub use sentinel_domain::ports::{
    FileSystemPort, GitStatusPort, ProcessOutput, ProcessPort, VectorPoint, VectorScrollResult,
    VectorSearchHit, VectorStorePort,
};

// ---------------------------------------------------------------------------
// HookContext — bundles all injected ports for hook functions
// ---------------------------------------------------------------------------

/// Context passed to all hook `process()` functions.
///
/// Bundles all injected ports so hook signatures stay stable as new
/// ports are added. Constructed once in the dispatcher (hook_cmd.rs).
pub struct HookContext<'a> {
    /// Git operations (branch, worktree, uncommitted changes).
    pub git: &'a dyn GitStatusPort,

    /// Vector database (Qdrant). `None` if not configured.
    pub vector_store: Option<&'a dyn VectorStorePort>,

    /// Filesystem operations. Always present.
    pub fs: &'a dyn FileSystemPort,

    /// Process execution (run commands, spawn detached). Always present.
    pub process: &'a dyn ProcessPort,
}

/// Test utilities for creating mock `HookContext`.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::path::{Path, PathBuf};

    pub struct StubGit;
    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> { Ok(vec![]) }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> { Ok("main".into()) }
        fn is_worktree(&self, _: &str) -> bool { false }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> { Ok(false) }
    }

    pub struct StubFs;
    impl FileSystemPort for StubFs {
        fn home_dir(&self) -> Option<PathBuf> { Some(PathBuf::from("/mock/home")) }
        fn read_to_string(&self, _: &Path) -> anyhow::Result<String> { anyhow::bail!("not found") }
        fn write(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
        fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> { Ok(()) }
        fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
        fn exists(&self, _: &Path) -> bool { false }
        fn is_dir(&self, _: &Path) -> bool { false }
        fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> { anyhow::bail!("no") }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

    pub struct StubProcess;
    impl ProcessPort for StubProcess {
        fn run(&self, _: &str, _: &[&str], _: Option<&str>) -> anyhow::Result<ProcessOutput> {
            Ok(ProcessOutput { success: true, stdout: String::new(), stderr: String::new() })
        }
        fn spawn_detached(&self, _: &str, _: &[&str]) -> anyhow::Result<()> { Ok(()) }
    }

    /// Create a test `HookContext` with stub ports.
    pub fn stub_ctx() -> HookContext<'static> {
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let fs: &'static StubFs = Box::leak(Box::new(StubFs));
        let process: &'static StubProcess = Box::leak(Box::new(StubProcess));
        HookContext { git, vector_store: None, fs, process }
    }
}
