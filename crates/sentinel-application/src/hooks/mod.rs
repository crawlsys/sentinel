//! Hook Implementations
//!
//! All hooks run through the sentinel Rust engine.
//! Each module implements one hook. Add new hooks here and
//! update `HOOK_NAMES` to keep the count accurate.

pub mod account_cascade;
pub mod activity_tracker;
pub mod agent_revocation;
pub mod ask_question_resync_nudge;
pub mod audit_extract;
pub mod autocron;
mod block_context;
pub mod bug_task_gate;
pub mod build_auto_monitor;
pub mod build_notify;
pub mod claim_reality_check;
pub mod commit_hygiene;
pub mod commit_message_validator;
pub mod context_monitor;
pub mod cwd_changed;
pub mod db_ops_gate;
pub mod dep_check;
pub mod doc_cleanup;
pub mod doc_drift;
pub mod doppler_auth0_gate;
pub mod dry_run_then_commit;
pub mod error_reporter;
pub mod execution_log;
pub mod git_hygiene;
pub mod good_citizen_observer;
pub mod hookdeck_decoders;
pub mod hygiene_override;
pub mod hygiene_reminders;
pub mod linear_inbound_sync;
pub mod linear_lifecycle;
pub mod linear_pm_gate;
pub mod mcp_health;
pub mod memory_extract;
pub mod memory_feedback;
pub mod memory_inject;
pub mod memory_provision;
pub mod memory_turn_capture;
pub mod memory_verify;
pub mod orchestration_nudge;
pub mod permission_denied;
pub mod phase_gate;
pub mod phase_validator;
pub mod plan_organizer;
pub mod plan_title_gate;
pub mod post_compact;
pub mod pr_auto_monitor;
pub mod pr_merge_gate;
pub mod pre_commit_verification;
pub mod pre_compact;
pub mod pre_push_browser_test;
pub mod production_action_notice;
pub mod production_override;
pub mod project_scope_tracker;
pub mod prompt_injection_nudge;
pub mod provenance_validate;
pub mod requirements_traceability_gate;
pub mod self_annealing;
pub mod session_end;
pub mod session_index;
pub mod session_init;
pub mod session_summary;
pub mod setup;
pub mod skill_invocation_gate;
pub mod skill_router;
pub mod skill_telemetry;
pub mod spec_challenge_gate;
pub mod step_anomaly;
pub mod step_gate;
pub mod step_judge;
pub mod stop_failure;
pub mod subagent_start;
pub mod subagent_stop;
pub mod task_completed;
pub mod task_coverage_check;
pub mod task_created;
pub mod task_decomposition_gate;
pub mod task_persist;
pub mod task_rehydrate;
pub mod task_status_line;
pub mod tasks_md_guard;
pub mod teammate_idle;
pub mod test_evidence_recorder;
pub mod ticket_quality_gate;
pub mod todo_interceptor;
pub mod todo_loader;
pub mod tool_usage_gate;
pub mod upstream_freshness;
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

/// Normalize a filesystem path string so that different textual spellings of
/// the *same* directory produce the same string (and therefore the same
/// `project_hash`).
///
/// Two spellings observed in the wild for one dir:
/// `C:\Users\…\sentinel` (hooks/native) and `C:/Users/…/sentinel`
/// (Bash/git/forward-slash callers). Without normalization they hash to
/// different project keys and scatter one project's tasks into two snapshots
/// (live-reproduced 2026-07-06). We therefore, before any further processing:
///   1. Replace all `\` with `/` (single canonical separator).
///   2. Lowercase a leading `X:` Windows drive letter (`C:` == `c:`).
///
/// Path casing *below* the drive letter is left untouched — Windows is
/// case-insensitive but preserving, and lowercasing the whole path could
/// collide two genuinely distinct case-sensitive paths on a case-sensitive
/// mount. The drive letter is the only segment with a guaranteed canonical
/// case, so it is the only one folded.
#[must_use]
pub fn normalize_path(cwd: &str) -> String {
    let mut s = cwd.replace('\\', "/");
    // Lowercase a leading drive letter: "C:/…" -> "c:/…".
    if let Some(rest) = s.strip_prefix(|c: char| c.is_ascii_alphabetic()) {
        if rest.starts_with(':') {
            let drive = s.as_bytes()[0].to_ascii_lowercase() as char;
            s = format!("{drive}{}", &s[1..]);
        }
    }
    s
}

/// Canonicalize a working-directory path so that worktrees collapse to their
/// parent repo. Worktrees live at `<repo>/.claude/worktrees/<name>/...`, and
/// without this collapse every worktree-switch produces a different
/// `project_hash`, breaking task rehydration across worktrees.
///
/// The path is first run through [`normalize_path`] (separator + drive-case
/// folding) so mixed-separator spellings of the same dir collapse. Then the
/// transform looks for the literal segment `/.claude/worktrees/` and strips
/// everything from that point onward, leaving the original repo root. Paths
/// that don't contain a worktree segment are returned normalized-but-unchanged.
#[must_use]
pub fn canonical_project_cwd(cwd: &str) -> String {
    const NEEDLE_FWD: &str = "/.claude/worktrees/";
    let normalized = normalize_path(cwd);
    if let Some(idx) = normalized.find(NEEDLE_FWD) {
        return normalized[..idx].to_string();
    }
    normalized
}

/// Compute the canonical 4-byte project hash (8 hex chars) for a working
/// directory. Worktrees of the same repo collapse to the same hash so that
/// persisted task lists, session indexes, and metrics all key off the repo
/// root, not the per-worktree path.
#[must_use]
pub fn project_hash(cwd: &str) -> String {
    use sha2::{Digest, Sha256};
    let canonical = canonical_project_cwd(cwd);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let result = hasher.finalize();
    use std::fmt::Write as _;
    result[..4].iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Return a validated session id that is safe to use as a durable state key.
///
/// Missing session identity is different from a real session. Hooks must not
/// manufacture `unknown`, `default`, or any other synthetic key for files that
/// later participate in Sentinel authority or LangGraph projection.
#[must_use]
pub fn session_path_component(session_id: &str) -> Option<&str> {
    let session_id = session_id.trim();
    if session_id.eq_ignore_ascii_case("unknown") || session_id.eq_ignore_ascii_case("default") {
        return None;
    }
    sentinel_domain::SessionId::validate(session_id).ok()?;
    Some(session_id)
}

/// Resolve a concrete, validated session id from a hook input.
#[must_use]
pub fn concrete_input_session_id(input: &sentinel_domain::events::HookInput) -> Option<&str> {
    input.session_id.as_deref().and_then(session_path_component)
}

/// Return `<home>/.claude/sentinel/metrics`.
///
/// All metric/telemetry JSONL files go here (previously `~/.claude/metrics/`).
pub fn metrics_dir(home: &std::path::Path) -> std::path::PathBuf {
    sentinel_dir(home).join("metrics")
}

/// The root holding Claude Code's native per-session task lists:
/// `<home>/.claude/tasks`. Each session gets a subdir of `N.json` task files
/// plus `.lock`/`.highwatermark` control files.
pub fn tasks_root(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".claude").join("tasks")
}

/// Canonical mapping from a hook's `session_id` to its on-disk task directory.
///
/// Claude Code does **not** name the task dir uniformly: depending on how the
/// session was started, the same `session_id` value passed to hooks (the full
/// UUID, e.g. `e2ea5630-3c79-409c-9ca4-423975a5a5fb`) is stored under EITHER
///   - `<tasks>/<session_id>/`                  (literal id), OR
///   - `<tasks>/session-<first-uuid-group>/`    (e.g. `session-e2ea5630/`).
///
/// Both forms are emitted concurrently (observed live on the same day), so this
/// is not a migration that settled on one name — a hook must resolve whichever
/// the harness actually wrote for this session.
///
/// Every task hook previously hard-coded `tasks_root().join(session_id)`, which
/// silently missed the `session-…` form and made gates fail closed / coverage
/// checks read nothing. This is the single source of truth: it returns the
/// directory that EXISTS for this session, checked in a fixed, session-scoped
/// order (no cross-session scan, no mtime guessing). If neither exists yet it
/// returns the literal-id path so callers' own `is_dir` check still yields a
/// definitive "no tasks here" for a genuinely fresh session.
///
/// Idempotent on an already-prefixed id (one that already starts `session-`):
/// the prefixed candidate collapses to the same value, so passing either form
/// resolves correctly.
pub fn session_task_dir(
    fs: &dyn FileSystemPort,
    home: &std::path::Path,
    session_id: &str,
) -> std::path::PathBuf {
    let root = tasks_root(home);
    let literal = root.join(session_id);
    if fs.is_dir(&literal) {
        return literal;
    }
    if let Some(prefixed_name) = session_prefixed_dir_name(session_id) {
        let prefixed = root.join(&prefixed_name);
        if fs.is_dir(&prefixed) {
            return prefixed;
        }
    }
    // Neither form exists yet — return the literal path so the caller's own
    // existence check reports a definitive absence (fresh session, no tasks).
    literal
}

/// The `session-<first-uuid-group>` directory name for a `session_id`, or
/// `None` when the id is unusable (empty) or already in prefixed form (so the
/// resolver doesn't double-prefix). The "first group" is the substring before
/// the first `-`, matching how Claude Code shortens the UUID.
fn session_prefixed_dir_name(session_id: &str) -> Option<String> {
    if session_id.is_empty() || session_id.starts_with("session-") {
        return None;
    }
    let first_group = session_id.split('-').next().unwrap_or(session_id);
    if first_group.is_empty() {
        return None;
    }
    Some(format!("session-{first_group}"))
}

/// Return `<home>/.claude/sentinel/persistent-tasks`.
///
/// Snapshots of the per-session `TaskList` (one subdir per `project_hash`). The
/// authoritative source for `task_rehydrate` on `SessionStart`. This is the
/// only supported persistent task snapshot root.
pub fn persistent_tasks_root(home: &std::path::Path) -> std::path::PathBuf {
    sentinel_dir(home).join("persistent-tasks")
}

/// All hook module names — used for dynamic counting.
/// Keep in sync with the `pub mod` declarations above.
pub const HOOK_NAMES: &[&str] = &[
    "account_cascade",
    "activity_tracker",
    "agent_revocation",
    "ask_question_resync_nudge",
    "audit_extract",
    "autocron",
    "bug_task_gate",
    "build_auto_monitor",
    "build_notify",
    "claim_reality_check",
    "commit_hygiene",
    "commit_message_validator",
    "context_monitor",
    "cwd_changed",
    "db_ops_gate",
    "dep_check",
    "doc_cleanup",
    "doc_drift",
    "doppler_auth0_gate",
    "dry_run_then_commit",
    "error_reporter",
    "execution_log",
    "git_hygiene",
    "good_citizen_observer",
    "hygiene_override",
    "hygiene_reminders",
    "linear_inbound_sync",
    "linear_lifecycle",
    "linear_pm_gate",
    "mcp_health",
    "memory_extract",
    "memory_feedback",
    "memory_inject",
    "memory_provision",
    "memory_turn_capture",
    "memory_verify",
    "orchestration_nudge",
    "permission_denied",
    "phase_gate",
    "phase_validator",
    "plan_organizer",
    "plan_title_gate",
    "post_compact",
    "pr_auto_monitor",
    "pr_merge_gate",
    "pre_commit_verification",
    "pre_compact",
    "pre_push_browser_test",
    "production_action_notice",
    "production_override",
    "project_scope_tracker",
    "prompt_injection_nudge",
    "provenance_validate",
    "requirements_traceability_gate",
    "spec_challenge_gate",
    "self_annealing",
    "session_end",
    "session_index",
    "session_init",
    "session_summary",
    "setup",
    "skill_invocation_gate",
    "skill_router",
    "skill_telemetry",
    "step_anomaly",
    "step_gate",
    "step_judge",
    "stop_failure",
    "subagent_start",
    "subagent_stop",
    "task_completed",
    "task_coverage_check",
    "task_decomposition_gate",
    "task_created",
    "task_persist",
    "task_rehydrate",
    "task_status_line",
    "tasks_md_guard",
    "teammate_idle",
    "test_evidence_recorder",
    "ticket_quality_gate",
    "todo_interceptor",
    "todo_loader",
    "tool_usage_gate",
    "upstream_freshness",
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
/// Most hook work must be quick (the default 3s budget). For the few paths
/// where a slightly-slower-but-must-complete call is acceptable (e.g. recall
/// search, which cold-starts memory-mcp + embeds + vector-searches and can
/// exceed 3s), use [`run_async_timeout`] with an explicit budget.
pub fn run_async<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T> + Send,
    T: Send + Default,
{
    run_async_timeout(future, RUN_ASYNC_TIMEOUT)
}

/// Like [`run_async`] but with an explicit wall-clock timeout. Use for hook
/// work that legitimately needs more than the default 3s (recall search, etc.)
/// — never for the hot blocking path of a gate.
pub fn run_async_timeout<F, T>(future: F, timeout: std::time::Duration) -> T
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
                    if let Ok(result) = tokio::time::timeout(timeout, future).await {
                        result
                    } else {
                        tracing::debug!("run_async: wall-clock timeout exceeded");
                        T::default()
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
    EnvPort, FileSystemPort, GitStatusPort, LinearLookupError, LinearLookupPort, LlmModel, LlmPort,
    LlmRequest, MemoryMcpPort, ProcessOutput, ProcessPort, VectorPoint, VectorScrollResult,
    VectorStorePort,
};

// ---------------------------------------------------------------------------
// HookContext — bundles all injected ports for hook functions
// ---------------------------------------------------------------------------

/// Context passed to all hook `process()` functions.
///
/// Bundles all injected ports so hook signatures stay stable as new
/// ports are added. Constructed once in the dispatcher (`hook_cmd.rs`).
pub struct HookContext<'a> {
    /// Git operations (branch, worktree, uncommitted changes).
    pub git: &'a dyn GitStatusPort,

    /// Vector database (Qdrant). `None` if not configured.
    pub vector_store: Option<&'a dyn VectorStorePort>,

    /// Filesystem operations. Always present.
    pub fs: &'a dyn FileSystemPort,

    /// Process execution (run commands, spawn detached). Always present.
    pub process: &'a dyn ProcessPort,

    /// LLM completion through the standardized OpenRouter adapter. `None` if
    /// no API key is configured.
    pub llm: Option<&'a dyn LlmPort>,

    /// Memory engine MCP client. Always present — wraps memory-mcp stdio.
    pub memory_mcp: &'a dyn MemoryMcpPort,

    /// Environment-variable reader. Always present — wraps `std::env`.
    pub env: &'a dyn EnvPort,

    /// Real-time single-issue Linear lookup for the PM gate. `None` means
    /// live Linear authority is unavailable, so targeted start attempts fail
    /// closed.
    pub linear_lookup: Option<&'a dyn LinearLookupPort>,
}

impl HookContext<'_> {
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
    /// Used by `pr_merge_gate`, `doppler_auth0_gate`, `task_rehydrate`.
    pub fn autopilot_enabled(&self) -> bool {
        match self.env.var("SENTINEL_AUTOPILOT").as_deref() {
            None | Some("" | "0") => false,
            Some(_) => true,
        }
    }
}

/// Test utilities for creating mock `HookContext`.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use sentinel_domain::port_errors::{FileSystemError, GitError, ProcessError};
    use std::path::{Path, PathBuf};

    pub struct StubGit;
    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _: &str) -> Result<bool, GitError> {
            Ok(false)
        }
        fn changed_files(&self, _: &str) -> Result<Vec<String>, GitError> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> Result<String, GitError> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> Result<bool, GitError> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn rev_list_count_range(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn head_sha(&self, _: &str) -> Option<String> {
            None
        }
    }

    pub struct StubFs;
    impl FileSystemPort for StubFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/mock/home"))
        }
        fn read_to_string(&self, _: &Path) -> Result<String, FileSystemError> {
            Err(FileSystemError::NotFound("not found".into()))
        }
        fn write(&self, _: &Path, _: &[u8]) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn replace_file_atomic(&self, _: &Path, _: &[u8]) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn create_dir_all(&self, _: &Path) -> Result<(), FileSystemError> {
            Ok(())
        }
        fn read_dir(&self, _: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
            Ok(vec![])
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn metadata(&self, _: &Path) -> Result<std::fs::Metadata, FileSystemError> {
            Err(FileSystemError::Backend("no".into()))
        }
        fn append(&self, _: &Path, _: &[u8]) -> Result<(), FileSystemError> {
            Ok(())
        }
    }

    pub struct TestHomeFs {
        home: PathBuf,
    }

    impl TestHomeFs {
        pub fn new(home: impl Into<PathBuf>) -> Self {
            Self { home: home.into() }
        }
    }

    impl FileSystemPort for TestHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }

        fn read_to_string(&self, path: &Path) -> Result<String, FileSystemError> {
            Ok(std::fs::read_to_string(path)?)
        }

        fn write(&self, path: &Path, content: &[u8]) -> Result<(), FileSystemError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Ok(std::fs::write(path, content)?)
        }

        fn replace_file_atomic(&self, path: &Path, content: &[u8]) -> Result<(), FileSystemError> {
            self.write(path, content)
        }

        fn create_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
            Ok(std::fs::create_dir_all(path)?)
        }

        fn read_dir(&self, path: &Path) -> Result<Vec<PathBuf>, FileSystemError> {
            Ok(std::fs::read_dir(path)?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect())
        }

        fn exists(&self, path: &Path) -> bool {
            path.exists()
        }

        fn is_dir(&self, path: &Path) -> bool {
            path.is_dir()
        }

        fn metadata(&self, path: &Path) -> Result<std::fs::Metadata, FileSystemError> {
            Ok(std::fs::metadata(path)?)
        }

        fn append(&self, path: &Path, content: &[u8]) -> Result<(), FileSystemError> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(content)?;
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> Result<(), FileSystemError> {
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err.into()),
            }
        }

        fn remove_dir_all(&self, path: &Path) -> Result<(), FileSystemError> {
            match std::fs::remove_dir_all(path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err.into()),
            }
        }
    }

    pub struct StubProcess;
    impl ProcessPort for StubProcess {
        fn run(&self, _: &str, _: &[&str], _: Option<&str>) -> Result<ProcessOutput, ProcessError> {
            Ok(ProcessOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
        fn spawn_detached(&self, _: &str, _: &[&str]) -> Result<(), ProcessError> {
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
        ) -> Result<serde_json::Value, sentinel_domain::port_errors::MemoryMcpError> {
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
        HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
            linear_lookup: None,
        }
    }

    pub fn stub_ctx_with_fs<'a>(fs: &'a dyn FileSystemPort) -> HookContext<'a> {
        let base = stub_ctx();
        HookContext {
            git: base.git,
            vector_store: base.vector_store,
            fs,
            process: base.process,
            llm: base.llm,
            memory_mcp: base.memory_mcp,
            env: base.env,
            linear_lookup: base.linear_lookup,
        }
    }
}

#[cfg(test)]
mod persistent_tasks_root_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn persistent_tasks_root_is_under_sentinel() {
        let home = PathBuf::from("/some/home");
        let root = persistent_tasks_root(&home);
        assert!(
            root.ends_with(".claude/sentinel/persistent-tasks")
                || root.ends_with(r".claude\sentinel\persistent-tasks"),
            "got: {}",
            root.display()
        );
    }

    /// The canonical session-id validator every hook now delegates to. Pins the
    /// contract that made the dialect-collapse a security *hardening*: the loose
    /// copies (in phase_gate / hygiene_override / pre_commit_verification) used
    /// to accept `default`, path-traversal, oversized, and unsafe-char ids —
    /// all of which could key durable override/authority state. This asserts
    /// they are now rejected.
    #[test]
    fn session_path_component_rejects_unsafe_ids() {
        // Accepts a real id.
        assert_eq!(session_path_component("abc-123_DEF"), Some("abc-123_DEF"));
        assert_eq!(session_path_component("  trimmed-id  "), Some("trimmed-id"));
        // Synthetic placeholders — rejected (case-insensitive).
        assert_eq!(session_path_component("unknown"), None);
        assert_eq!(session_path_component("UNKNOWN"), None);
        assert_eq!(session_path_component("default"), None);
        assert_eq!(session_path_component("Default"), None);
        assert_eq!(session_path_component(""), None);
        assert_eq!(session_path_component("   "), None);
        // Path traversal — the check the old inline copies LACKED.
        assert_eq!(session_path_component("../etc/passwd"), None);
        assert_eq!(session_path_component("a..b"), None);
        // Unsafe characters.
        assert_eq!(session_path_component("has/slash"), None);
        assert_eq!(session_path_component("has space"), None);
        // Oversized (> 128).
        assert_eq!(session_path_component(&"a".repeat(129)), None);
        assert_eq!(
            session_path_component(&"a".repeat(128)).map(str::len),
            Some(128)
        );
    }
}

#[cfg(test)]
mod project_hash_tests {
    use super::*;

    #[test]
    fn non_worktree_path_is_unchanged() {
        assert_eq!(canonical_project_cwd("/repo"), "/repo");
        assert_eq!(
            canonical_project_cwd("/some/deep/path/with/no/worktrees"),
            "/some/deep/path/with/no/worktrees"
        );
        assert_eq!(canonical_project_cwd(""), "");
    }

    #[test]
    fn forward_slash_worktree_collapses_to_repo_root() {
        assert_eq!(
            canonical_project_cwd("/repo/.claude/worktrees/feat-x"),
            "/repo"
        );
        assert_eq!(
            canonical_project_cwd("/repo/.claude/worktrees/feat-x/crates/sentinel-domain"),
            "/repo"
        );
    }

    #[test]
    fn backslash_worktree_collapses_to_repo_root() {
        // Output is now separator-normalized (backslashes → forward, drive
        // lowercased) so mixed spellings of one dir collapse to one key.
        assert_eq!(
            canonical_project_cwd(r"C:\repo\.claude\worktrees\feat-x"),
            "c:/repo"
        );
        assert_eq!(
            canonical_project_cwd(r"C:\repo\.claude\worktrees\feat-x\crates\foo"),
            "c:/repo"
        );
    }

    #[test]
    fn normalize_path_folds_separators_and_drive_case() {
        assert_eq!(normalize_path(r"C:\Users\g\repo"), "c:/Users/g/repo");
        assert_eq!(normalize_path("C:/Users/g/repo"), "c:/Users/g/repo");
        assert_eq!(normalize_path(r"c:/Users\g/repo"), "c:/Users/g/repo"); // mixed
                                                                           // Non-Windows paths: only separator folding (no drive letter).
        assert_eq!(normalize_path("/home/g/repo"), "/home/g/repo");
        // Path casing below the drive is preserved (not folded).
        assert_eq!(normalize_path(r"C:\Users\MixedCase"), "c:/Users/MixedCase");
    }

    #[test]
    fn same_dir_different_spellings_produce_same_project_hash() {
        // The live-reproduced bug: C:\...\sentinel and C:/.../sentinel hashed
        // to different keys (c08fea48 vs fdf9d8fd), scattering one project's
        // tasks into two snapshots. All spellings must now collapse.
        let back = project_hash(r"C:\Users\garys\Documents\GitHub\sentinel");
        let fwd = project_hash("C:/Users/garys/Documents/GitHub/sentinel");
        let lower = project_hash("c:/Users/garys/Documents/GitHub/sentinel");
        let mixed = project_hash(r"c:/Users\garys/Documents\GitHub/sentinel");
        assert_eq!(back, fwd, "backslash vs forward-slash must match");
        assert_eq!(fwd, lower, "drive-letter case must not matter");
        assert_eq!(lower, mixed, "mixed separators must match");
        // And a worktree of it collapses to the same hash too.
        let wt = project_hash(r"C:\Users\garys\Documents\GitHub\sentinel\.claude\worktrees\feat-x");
        assert_eq!(back, wt, "worktree collapses to the same project hash");
    }

    #[test]
    fn worktree_collapse_invariant_holds_for_project_hash() {
        // The whole point: main repo and any worktree of it produce the same hash.
        let main = "/Users/operator/Documents/GitHub/sentinel";
        let wt_a = "/Users/operator/Documents/GitHub/sentinel/.claude/worktrees/feat-stepproof";
        let wt_b =
            "/Users/operator/Documents/GitHub/sentinel/.claude/worktrees/feat-other/crates/x";
        assert_eq!(project_hash(main), project_hash(wt_a));
        assert_eq!(project_hash(main), project_hash(wt_b));
    }

    #[test]
    fn project_hash_distinguishes_different_repos() {
        let a = "/Users/operator/Documents/GitHub/sentinel";
        let b = "/Users/operator/Documents/GitHub/twilio-mcp-rust";
        assert_ne!(project_hash(a), project_hash(b));
    }

    #[test]
    fn project_hash_format_is_8_hex_chars() {
        let h = project_hash("/repo");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "got: {h}");
    }
}

#[cfg(test)]
mod session_task_dir_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// FS whose `home_dir` is a caller-supplied temp dir; `is_dir` hits the real
    /// filesystem so resolution can be exercised against seeded directories.
    struct TmpHomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for TmpHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            _: &Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

    // --- pure name derivation ------------------------------------------------

    #[test]
    fn prefixed_name_from_full_uuid_uses_first_group() {
        assert_eq!(
            session_prefixed_dir_name("e2ea5630-3c79-409c-9ca4-423975a5a5fb").as_deref(),
            Some("session-e2ea5630")
        );
        assert_eq!(
            session_prefixed_dir_name("dac1d29b-1111-2222-3333-444455556666").as_deref(),
            Some("session-dac1d29b")
        );
    }

    #[test]
    fn prefixed_name_is_idempotent_and_rejects_empty() {
        // Already-prefixed id must not be double-prefixed.
        assert_eq!(session_prefixed_dir_name("session-e2ea5630"), None);
        // Empty id is unusable.
        assert_eq!(session_prefixed_dir_name(""), None);
    }

    #[test]
    fn prefixed_name_for_id_without_hyphen_uses_whole_id() {
        assert_eq!(
            session_prefixed_dir_name("abcdef12").as_deref(),
            Some("session-abcdef12")
        );
    }

    // --- directory resolution ------------------------------------------------

    fn seed_dir(home: &Path, name: &str) {
        std::fs::create_dir_all(tasks_root(home).join(name)).unwrap();
    }

    #[test]
    fn resolves_literal_dir_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "e2ea5630-3c79-409c-9ca4-423975a5a5fb";
        seed_dir(tmp.path(), id);
        let fs = TmpHomeFs {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(
            session_task_dir(&fs, tmp.path(), id),
            tasks_root(tmp.path()).join(id)
        );
    }

    #[test]
    fn resolves_session_prefixed_dir_when_literal_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "e2ea5630-3c79-409c-9ca4-423975a5a5fb";
        seed_dir(tmp.path(), "session-e2ea5630");
        let fs = TmpHomeFs {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(
            session_task_dir(&fs, tmp.path(), id),
            tasks_root(tmp.path()).join("session-e2ea5630"),
            "must fall through to the session-{{first8}} dir the harness wrote"
        );
    }

    #[test]
    fn prefers_literal_over_prefixed_when_both_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "e2ea5630-3c79-409c-9ca4-423975a5a5fb";
        seed_dir(tmp.path(), id);
        seed_dir(tmp.path(), "session-e2ea5630");
        let fs = TmpHomeFs {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(
            session_task_dir(&fs, tmp.path(), id),
            tasks_root(tmp.path()).join(id),
            "literal id is the primary form and wins when both are present"
        );
    }

    #[test]
    fn returns_literal_path_when_neither_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "ffffffff-0000-0000-0000-000000000000";
        let fs = TmpHomeFs {
            home: tmp.path().to_path_buf(),
        };
        // No dir seeded — returns the literal path so the caller's is_dir check
        // reports a definitive absence (fresh session).
        assert_eq!(
            session_task_dir(&fs, tmp.path(), id),
            tasks_root(tmp.path()).join(id)
        );
    }
}
