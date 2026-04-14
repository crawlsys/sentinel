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
pub mod memory_feedback;
pub mod memory_inject;
pub mod memory_verify;
pub mod permission_denied;
pub mod phase_gate;
pub mod phase_validator;
pub mod plan_organizer;
pub mod post_compact;
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
pub mod verification_gate;
pub mod worktree_reminder;
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
    "memory_feedback",
    "memory_inject",
    "memory_verify",
    "permission_denied",
    "phase_gate",
    "phase_validator",
    "plan_organizer",
    "post_compact",
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
    "verification_gate",
    "worktree_reminder",
    "wrangler_guard",
];

// ---------------------------------------------------------------------------
// Shared async runtime helper
// ---------------------------------------------------------------------------

/// Run an async block safely, whether or not we're already inside a tokio runtime.
///
/// When sentinel hooks are invoked from the async CLI dispatcher (`hook_cmd::run`),
/// a tokio runtime is already active. Creating a nested runtime panics with
/// "Cannot start a runtime from within a runtime". This helper detects that case
/// and spawns a scoped thread with its own runtime instead.
///
/// Used by all memory/Qdrant hooks that need to make async HTTP calls.
pub fn run_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send + Default,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        // Already inside a runtime — run on a scoped thread to avoid nesting.
        // `std::thread::scope` guarantees the thread joins before borrowed data
        // goes out of scope, so the future can safely reference the caller's stack.
        std::thread::scope(|s| {
            s.spawn(|| {
                match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(future),
                    Err(_) => T::default(),
                }
            })
            .join()
            .unwrap_or_default()
        })
    } else {
        // No runtime — safe to create one directly.
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(future),
            Err(_) => T::default(),
        }
    }
}

/// Port for git status queries — implemented by the infrastructure layer.
/// The git-dependent hooks accept this trait so the application layer
/// stays decoupled from infrastructure (no cyclic dependency).
// ---------------------------------------------------------------------------
// Hexagonal ports — traits for external IO dependencies
// ---------------------------------------------------------------------------
// Infrastructure implementations live in sentinel-infrastructure.
// The CLI (hook_cmd.rs) constructs concrete adapters and injects them.

pub trait GitStatusPort {
    /// Check if there are uncommitted changes in the given repo path.
    fn has_uncommitted_changes(&self, repo_path: &str) -> anyhow::Result<bool>;

    /// Get list of changed files (staged + unstaged).
    fn changed_files(&self, repo_path: &str) -> anyhow::Result<Vec<String>>;

    /// Get the current branch name (e.g. "main", "feat/my-feature").
    fn current_branch(&self, repo_path: &str) -> anyhow::Result<String>;

    /// Check if the path is inside a git worktree (not the main working tree).
    fn is_worktree(&self, repo_path: &str) -> bool;
}

/// Port for vector database operations (Qdrant).
///
/// Abstracts all HTTP calls to Qdrant Cloud. Memory hooks use this for
/// search, upsert, scroll, payload updates. The infrastructure layer
/// handles auth, URL construction, and HTTP client lifecycle.
///
/// All methods are async — the infrastructure implementation uses reqwest.
/// Hook callers must wrap in a tokio runtime (or use `run_async_block`).
#[async_trait::async_trait]
pub trait VectorStorePort: Send + Sync {
    /// Semantic search by text query. Returns (score, payload_json) pairs.
    async fn query(
        &self,
        collection: &str,
        query_text: &str,
        limit: u32,
        min_score: f64,
    ) -> anyhow::Result<Vec<VectorSearchHit>>;

    /// Upsert points with server-side embedding. Each point has an id,
    /// text (for embedding), and a JSON payload.
    async fn upsert_points(
        &self,
        collection: &str,
        points: Vec<VectorPoint>,
    ) -> anyhow::Result<()>;

    /// Scroll (list) points with optional filter. Returns payloads.
    async fn scroll(
        &self,
        collection: &str,
        filter: Option<serde_json::Value>,
        limit: u32,
    ) -> anyhow::Result<Vec<VectorScrollResult>>;

    /// Update payload fields on existing points by ID.
    async fn set_payload(
        &self,
        collection: &str,
        point_ids: &[String],
        payload: serde_json::Value,
    ) -> anyhow::Result<()>;

    /// Get points by IDs with payload.
    async fn get_points(
        &self,
        collection: &str,
        ids: &[String],
        payload_fields: &[&str],
    ) -> anyhow::Result<Vec<VectorScrollResult>>;
}

/// A single search result from a vector query.
#[derive(Debug, Clone)]
pub struct VectorSearchHit {
    pub id: String,
    pub score: f64,
    pub payload: serde_json::Value,
}

/// A point to upsert (with server-side embedding).
#[derive(Debug, Clone)]
pub struct VectorPoint {
    pub id: String,
    pub text: String,
    pub payload: serde_json::Value,
}

/// A point returned from scroll or get_points.
#[derive(Debug, Clone)]
pub struct VectorScrollResult {
    pub id: String,
    pub payload: serde_json::Value,
}

/// Port for filesystem operations.
///
/// Abstracts all std::fs and dirs calls. Hooks use this for reading/writing
/// state files, config, metrics, and memory files. The infrastructure layer
/// delegates to real std::fs. Tests can inject a mock.
pub trait FileSystemPort: Send + Sync {
    /// Get the user's home directory.
    fn home_dir(&self) -> Option<std::path::PathBuf>;

    /// Read a file's contents as a string.
    fn read_to_string(&self, path: &std::path::Path) -> anyhow::Result<String>;

    /// Write bytes to a file (creates parent dirs if needed).
    fn write(&self, path: &std::path::Path, content: &[u8]) -> anyhow::Result<()>;

    /// Create a directory and all parent directories.
    fn create_dir_all(&self, path: &std::path::Path) -> anyhow::Result<()>;

    /// List entries in a directory (returns paths).
    fn read_dir(&self, path: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>>;

    /// Check if a path exists.
    fn exists(&self, path: &std::path::Path) -> bool;

    /// Check if a path is a directory.
    fn is_dir(&self, path: &std::path::Path) -> bool;

    /// Get file metadata (for mtime checks).
    fn metadata(&self, path: &std::path::Path) -> anyhow::Result<std::fs::Metadata>;

    /// Append bytes to a file (creates if needed, does not truncate).
    fn append(&self, path: &std::path::Path, content: &[u8]) -> anyhow::Result<()>;
}

/// Port for spawning external processes.
///
/// Abstracts std::process::Command calls for binary execution and
/// fire-and-forget spawns. Used by session_init (qdrant sync, git),
/// wrangler_guard (dialog binary), pre_push_steel_test (git).
pub trait ProcessPort: Send + Sync {
    /// Run a command and capture output. Returns (exit_success, stdout, stderr).
    fn run(
        &self,
        command: &str,
        args: &[&str],
        cwd: Option<&str>,
    ) -> anyhow::Result<ProcessOutput>;

    /// Spawn a detached process (fire-and-forget). Returns immediately.
    fn spawn_detached(
        &self,
        command: &str,
        args: &[&str],
    ) -> anyhow::Result<()>;
}

/// Output from a process execution.
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

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
