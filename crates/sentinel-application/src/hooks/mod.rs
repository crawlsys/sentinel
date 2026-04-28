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
pub mod dep_check;
pub mod doc_cleanup;
pub mod doc_drift;
pub mod doppler_auth0_gate;
pub mod error_reporter;
pub mod evidence_collector;
pub mod execution_log;
pub mod git_hygiene;
pub mod hookdeck_decoders;
pub mod hygiene_override;
pub mod hygiene_reminders;
pub mod linear_lifecycle;
pub mod mcp_health;
pub mod memory_extract;
pub mod memory_feedback;
pub mod memory_inject;
pub mod memory_verify;
pub mod orchestration_nudge;
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
pub mod task_coverage_check;
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
    "dep_check",
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
    "task_coverage_check",
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
/// Re-exported from `sentinel_domain::constants::RUN_ASYNC_TIMEOUT` for the
/// existing call-site name; the domain owns the value so it can stay in sync
/// with related timeouts (`API_CALL_TIMEOUT`, `VECTOR_BATCH_TIMEOUT`).
const RUN_ASYNC_TIMEOUT: std::time::Duration = sentinel_domain::constants::RUN_ASYNC_TIMEOUT;

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
    EnvPort, FileSystemPort, GitStatusPort, LlmModel, LlmPort, LlmRequest, MemoryMcpPort,
    ProcessOutput, ProcessPort, VectorPoint, VectorScrollResult, VectorStorePort,
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

    /// LLM completion (Anthropic). `None` if no API key is configured.
    pub llm: Option<&'a dyn LlmPort>,

    /// Memory engine MCP client. Always present — wraps memory-mcp stdio.
    pub memory_mcp: &'a dyn MemoryMcpPort,

    /// Environment-variable reader. Always present — wraps `std::env`.
    pub env: &'a dyn EnvPort,
}

impl<'a> HookContext<'a> {
    /// Resolve the active Claude session ID by reading `CLAUDE_SESSION_ID`
    /// then falling back to `SESSION_ID`. Five hooks (`activity_tracker`,
    /// `commit_hygiene`, `context_monitor`, `doc_drift`, `verification_gate`)
    /// open with the same two-line idiom — this collapses it to one call.
    pub fn session_id(&self) -> Option<String> {
        self.env
            .var("CLAUDE_SESSION_ID")
            .or_else(|| self.env.var("SESSION_ID"))
    }

    /// Whether sentinel autopilot is active. Read from `SENTINEL_AUTOPILOT`
    /// (any value other than `"0"` / empty / unset is treated as enabled).
    /// Used by `tool_usage_gate`, `pr_merge_gate`, `doppler_auth0_gate`,
    /// `task_rehydrate`.
    pub fn autopilot_enabled(&self) -> bool {
        match self.env.var("SENTINEL_AUTOPILOT").as_deref() {
            None | Some("") | Some("0") => false,
            Some(_) => true,
        }
    }
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
        fn repo_root(&self, _: &str) -> Option<String> { None }
        fn list_worktree_names(&self, _: &str) -> Vec<String> { Vec::new() }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { None }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> { Vec::new() }
    }

    pub struct StubFs;
    impl FileSystemPort for StubFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/mock/home"))
        }
        fn read_to_string(&self, _: &Path) -> anyhow::Result<String> {
            anyhow::bail!("not found")
        }
        fn write(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(vec![])
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> {
            anyhow::bail!("no")
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    pub struct StubProcess;
    impl ProcessPort for StubProcess {
        fn run(&self, _: &str, _: &[&str], _: Option<&str>) -> anyhow::Result<ProcessOutput> {
            Ok(ProcessOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn spawn_detached(&self, _: &str, _: &[&str]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Stub `EnvPort` backed by an in-memory map. Tests inject scenarios
    /// like `StubEnv::with(&[("SENTINEL_AUTOPILOT", "1")])` without touching
    /// process-global env state.
    #[derive(Default)]
    pub struct StubEnv {
        vars: std::collections::HashMap<String, String>,
    }

    impl StubEnv {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with(pairs: &[(&str, &str)]) -> Self {
            let mut vars = std::collections::HashMap::new();
            for (k, v) in pairs {
                vars.insert((*k).to_string(), (*v).to_string());
            }
            Self { vars }
        }

        pub fn set(&mut self, key: &str, value: &str) {
            self.vars.insert(key.to_string(), value.to_string());
        }
    }

    impl EnvPort for StubEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn var_os(&self, key: &str) -> Option<std::ffi::OsString> {
            self.vars.get(key).map(std::ffi::OsString::from)
        }
    }

    pub struct StubMemoryMcp;
    #[async_trait::async_trait]
    impl MemoryMcpPort for StubMemoryMcp {
        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Map<String, serde_json::Value>,
        ) -> anyhow::Result<serde_json::Value> {
            // Tests that don't need a memory-mcp response get a benign empty
            // object — hooks that depend on specific shapes will fail to
            // deserialise, which is the right signal to inject a real stub.
            Ok(serde_json::Value::Object(serde_json::Map::new()))
        }
    }

    /// Create a test `HookContext` with stub ports.
    pub fn stub_ctx() -> HookContext<'static> {
        let git: &'static StubGit = Box::leak(Box::new(StubGit));
        let fs: &'static StubFs = Box::leak(Box::new(StubFs));
        let process: &'static StubProcess = Box::leak(Box::new(StubProcess));
        let memory_mcp: &'static StubMemoryMcp = Box::leak(Box::new(StubMemoryMcp));
        let env: &'static StubEnv = Box::leak(Box::new(StubEnv::new()));
        HookContext { git, vector_store: None, fs, process, llm: None, memory_mcp, env }
    }
}
