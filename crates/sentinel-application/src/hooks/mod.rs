//! Hook Implementations
//!
//! All hooks run through the sentinel Rust engine.
//! Each module implements one hook. Add new hooks here and
//! update `HOOK_NAMES` to keep the count accurate.

pub mod account_cascade;
pub mod activity_tracker;
pub mod agent_revocation;
pub mod audit_extract;
mod block_context;
pub mod bug_task_gate;
pub mod build_auto_monitor;
pub mod build_notify;
pub mod catastrophic_escalation;
pub mod claim_reality_check;
pub mod commit_hygiene;
pub mod commit_message_validator;
pub mod constitution_gate;
pub mod consul_inbox;
pub mod context_monitor;
pub mod cwd_changed;
pub mod db_ops_gate;
pub mod dep_check;
pub mod doc_cleanup;
pub mod doc_drift;
pub mod doppler_auth0_gate;
pub mod dry_run_then_commit;
pub mod error_reporter;
pub mod evidence_collector;
pub mod execution_log;
pub mod git_hygiene;
pub mod glass_break_gate;
pub mod good_citizen_observer;
pub mod hookdeck_decoders;
pub mod hygiene_override;
pub mod hygiene_reminders;
pub mod linear_inbound_sync;
pub mod linear_lifecycle;
pub mod mcp_health;
pub mod memory_extract;
pub mod memory_feedback;
pub mod memory_inject;
pub mod memory_turn_capture;
pub mod memory_verify;
pub mod orchestration_nudge;
pub mod output_compressor;
pub mod permission_denied;
pub mod phase_gate;
pub mod phase_validator;
pub mod plan_organizer;
pub mod post_compact;
pub mod pr_auto_monitor;
pub mod pr_merge_gate;
pub mod pre_commit_verification;
pub mod pre_compact;
pub mod production_action_notice;
pub mod production_override;
pub mod pre_push_browser_test;
pub mod prompt_injection_nudge;
pub mod provenance_validate;
pub mod requirements_traceability_gate;
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
pub mod task_decomposition_gate;
pub mod task_created;
pub mod task_persist;
pub mod task_rehydrate;
pub mod tasks_md_guard;
pub mod teammate_idle;
pub mod test_evidence_recorder;
pub mod todo_interceptor;
pub mod todo_loader;
pub mod tool_usage_gate;
pub mod upstream_block;
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

/// Canonicalize a working-directory path so that worktrees collapse to their
/// parent repo. Worktrees live at `<repo>/.claude/worktrees/<name>/...`, and
/// without this collapse every worktree-switch produces a different
/// `project_hash`, breaking task rehydration across worktrees.
///
/// The transform looks for the literal segment `/.claude/worktrees/` (or the
/// same with `\` separators on Windows) and strips everything from that point
/// onward, leaving the original repo root. Paths that don't contain a
/// worktree segment are returned unchanged.
#[must_use]
pub fn canonical_project_cwd(cwd: &str) -> String {
    const NEEDLE_FWD: &str = "/.claude/worktrees/";
    const NEEDLE_BWD: &str = r"\.claude\worktrees\";
    if let Some(idx) = cwd.find(NEEDLE_FWD) {
        return cwd[..idx].to_string();
    }
    if let Some(idx) = cwd.find(NEEDLE_BWD) {
        return cwd[..idx].to_string();
    }
    cwd.to_string()
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

/// Return `<home>/.claude/sentinel/metrics`.
///
/// All metric/telemetry JSONL files go here (previously `~/.claude/metrics/`).
pub fn metrics_dir(home: &std::path::Path) -> std::path::PathBuf {
    sentinel_dir(home).join("metrics")
}

/// Return `<home>/.claude/sentinel/persistent-tasks`.
///
/// Snapshots of the per-session `TaskList` (one subdir per `project_hash`). The
/// authoritative source for `task_rehydrate` on `SessionStart`. Previously
/// lived at `<home>/.claude/persistent-tasks/` — moved under `sentinel/` so
/// all sentinel-owned state is colocated.
///
/// Use [`legacy_persistent_tasks_root`] when reading old data during the
/// migration window.
pub fn persistent_tasks_root(home: &std::path::Path) -> std::path::PathBuf {
    sentinel_dir(home).join("persistent-tasks")
}

/// Return the legacy `<home>/.claude/persistent-tasks` path. Only used for
/// one-time migration on first read; new writes always go through
/// [`persistent_tasks_root`].
pub fn legacy_persistent_tasks_root(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".claude").join("persistent-tasks")
}

/// One-time migration of persistent-tasks data from the legacy location to
/// the sentinel-owned location. Idempotent: no-op when the new dir already
/// exists or the legacy dir doesn't.
///
/// Strategy: rename the legacy directory in place. If rename fails (e.g.
/// across mount points, or because something else holds a handle on
/// Windows), fall back to copying each `<hash>/` subdir individually and
/// best-effort removing the legacy entries we successfully copied. The
/// legacy root itself is left as an empty husk so users can confirm
/// migration happened, then delete by hand.
pub fn migrate_persistent_tasks_dir(fs: &dyn FileSystemPort, home: &std::path::Path) {
    let new_root = persistent_tasks_root(home);
    let legacy_root = legacy_persistent_tasks_root(home);
    if fs.is_dir(&new_root) {
        return;
    }
    if !fs.is_dir(&legacy_root) {
        return;
    }
    if let Some(parent) = new_root.parent() {
        let _ = fs.create_dir_all(parent);
    }
    if std::fs::rename(&legacy_root, &new_root).is_ok() {
        tracing::info!(
            from = %legacy_root.display(),
            to = %new_root.display(),
            "Migrated persistent-tasks dir"
        );
        return;
    }
    // Fallback: per-entry copy.
    if let Err(e) = fs.create_dir_all(&new_root) {
        tracing::warn!(error = %e, "Failed to create new persistent-tasks dir; aborting migration");
        return;
    }
    let entries = fs.read_dir(&legacy_root).unwrap_or_default();
    for entry in entries {
        let Some(name) = entry
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let dest = new_root.join(&name);
        if fs.exists(&dest) {
            continue;
        }
        if std::fs::rename(&entry, &dest).is_ok() {
            continue;
        }
        // Best-effort recursive copy as last resort. We use std::fs directly
        // since the FileSystemPort doesn't expose a tree-copy primitive and
        // this only runs once per machine.
        if let Err(e) = copy_dir_recursive(&entry, &dest) {
            tracing::warn!(
                error = %e,
                from = %entry.display(),
                to = %dest.display(),
                "Failed to migrate persistent-tasks subdir; leaving legacy copy in place"
            );
        }
    }
    tracing::info!(
        from = %legacy_root.display(),
        to = %new_root.display(),
        "Migrated persistent-tasks dir (per-entry fallback)"
    );
}

fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            std::fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// All hook module names — used for dynamic counting.
/// Keep in sync with the `pub mod` declarations above.
pub const HOOK_NAMES: &[&str] = &[
    "account_cascade",
    "activity_tracker",
    "agent_revocation",
    "audit_extract",
    "bug_task_gate",
    "build_auto_monitor",
    "catastrophic_escalation",
    "claim_reality_check",
    "commit_hygiene",
    "commit_message_validator",
    "constitution_gate",
    "consul_inbox",
    "context_monitor",
    "cwd_changed",
    "db_ops_gate",
    "dep_check",
    "doc_cleanup",
    "doc_drift",
    "doppler_auth0_gate",
    "dry_run_then_commit",
    "error_reporter",
    "evidence_collector",
    "execution_log",
    "git_hygiene",
    "good_citizen_observer",
    "hygiene_override",
    "hygiene_reminders",
    "linear_inbound_sync",
    "linear_lifecycle",
    "mcp_health",
    "memory_extract",
    "memory_feedback",
    "memory_inject",
    "memory_verify",
    "output_compressor",
    "permission_denied",
    "phase_gate",
    "phase_validator",
    "plan_organizer",
    "post_compact",
    "pr_auto_monitor",
    "pr_merge_gate",
    "pre_commit_verification",
    "pre_compact",
    "pre_push_browser_test",
    "production_action_notice",
    "production_override",
    "prompt_injection_nudge",
    "provenance_validate",
    "requirements_traceability_gate",
    "spec_challenge_gate",
    "session_end",
    "session_index",
    "session_init",
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
    "tasks_md_guard",
    "teammate_idle",
    "test_evidence_recorder",
    "todo_interceptor",
    "todo_loader",
    "tool_usage_gate",
    "upstream_block",
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
                    if let Ok(result) = tokio::time::timeout(timeout, future).await { result } else {
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
    EnvPort, FileSystemPort, GitStatusPort, LlmModel, LlmPort, LlmRequest, MemoryMcpPort,
    ProcessOutput, ProcessPort, VectorPoint, VectorScrollResult, VectorStorePort,
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

    /// LLM completion (Anthropic). `None` if no API key is configured.
    pub llm: Option<&'a dyn LlmPort>,

    /// Memory engine MCP client. Always present — wraps memory-mcp stdio.
    pub memory_mcp: &'a dyn MemoryMcpPort,

    /// Environment-variable reader. Always present — wraps `std::env`.
    pub env: &'a dyn EnvPort,
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
    /// Used by `tool_usage_gate`, `pr_merge_gate`, `doppler_auth0_gate`,
    /// `task_rehydrate`.
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
    use std::path::{Path, PathBuf};

    pub struct StubGit;
    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
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
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
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
        HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
        }
    }
}

#[cfg(test)]
mod migrate_tests {
    use super::*;
    use std::path::Path;
    use std::path::PathBuf;

    /// Real-FS adapter scoped to a caller-supplied home directory.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
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
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn migrate_moves_legacy_data_to_new_location() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let legacy = legacy_persistent_tasks_root(&home);
        std::fs::create_dir_all(legacy.join("abc12345")).unwrap();
        std::fs::write(
            legacy.join("abc12345").join("tasks.json"),
            r#"[{"id":"1","subject":"x","status":"pending","blockedBy":[],"blocks":[]}]"#,
        )
        .unwrap();
        let fs = ScopedHomeFs { home: home.clone() };

        migrate_persistent_tasks_dir(&fs, &home);

        let new_root = persistent_tasks_root(&home);
        assert!(new_root.is_dir(), "new root must exist after migration");
        assert!(
            new_root.join("abc12345").join("tasks.json").is_file(),
            "task data must land at the new path"
        );
        // Legacy root should be gone (rename succeeded), or at least empty.
        let legacy_still_present = legacy.is_dir();
        if legacy_still_present {
            assert!(
                std::fs::read_dir(&legacy).unwrap().next().is_none(),
                "if legacy root persists (fallback path), it must be empty"
            );
        }
    }

    #[test]
    fn migrate_is_noop_when_new_already_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let legacy = legacy_persistent_tasks_root(&home);
        let new_root = persistent_tasks_root(&home);
        std::fs::create_dir_all(legacy.join("abc")).unwrap();
        std::fs::write(legacy.join("abc").join("legacy.txt"), "legacy").unwrap();
        std::fs::create_dir_all(new_root.join("abc")).unwrap();
        std::fs::write(new_root.join("abc").join("new.txt"), "new").unwrap();
        let fs = ScopedHomeFs { home: home.clone() };

        migrate_persistent_tasks_dir(&fs, &home);

        // New data is untouched.
        assert!(new_root.join("abc").join("new.txt").is_file());
        // Legacy file is also untouched (we don't merge — first-mover wins).
        assert!(legacy.join("abc").join("legacy.txt").is_file());
    }

    #[test]
    fn migrate_is_noop_when_legacy_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let fs = ScopedHomeFs { home: home.clone() };
        // Neither dir exists — must not panic, must not create the new dir.
        migrate_persistent_tasks_dir(&fs, &home);
        assert!(!persistent_tasks_root(&home).exists());
    }

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

    #[test]
    fn legacy_persistent_tasks_root_unchanged() {
        let home = PathBuf::from("/some/home");
        let root = legacy_persistent_tasks_root(&home);
        assert!(
            root.ends_with(".claude/persistent-tasks")
                || root.ends_with(r".claude\persistent-tasks"),
            "got: {}",
            root.display()
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
        assert_eq!(
            canonical_project_cwd(r"C:\repo\.claude\worktrees\feat-x"),
            r"C:\repo"
        );
        assert_eq!(
            canonical_project_cwd(r"C:\repo\.claude\worktrees\feat-x\crates\foo"),
            r"C:\repo"
        );
    }

    #[test]
    fn worktree_collapse_invariant_holds_for_project_hash() {
        // The whole point: main repo and any worktree of it produce the same hash.
        let main = "/Users/gary/Documents/GitHub/sentinel";
        let wt_a = "/Users/gary/Documents/GitHub/sentinel/.claude/worktrees/feat-stepproof";
        let wt_b = "/Users/gary/Documents/GitHub/sentinel/.claude/worktrees/feat-other/crates/x";
        assert_eq!(project_hash(main), project_hash(wt_a));
        assert_eq!(project_hash(main), project_hash(wt_b));
    }

    #[test]
    fn project_hash_distinguishes_different_repos() {
        let a = "/Users/gary/Documents/GitHub/sentinel";
        let b = "/Users/gary/Documents/GitHub/twilio-mcp-rust";
        assert_ne!(project_hash(a), project_hash(b));
    }

    #[test]
    fn project_hash_format_is_8_hex_chars() {
        let h = project_hash("/repo");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()), "got: {h}");
    }
}
