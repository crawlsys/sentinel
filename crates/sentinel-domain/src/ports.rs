//! Hexagonal ports — trait definitions for external IO dependencies.
//!
//! These ports define the boundaries between the domain/application layers
//! and the infrastructure layer. Infrastructure implementations live in
//! `sentinel-infrastructure`. The CLI (`hook_cmd.rs`) constructs concrete
//! adapters and injects them.
//!
//! **Domain purity**: These traits contain no I/O themselves — they are
//! contracts that the infrastructure layer fulfills.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Git port
// ---------------------------------------------------------------------------

/// Port for git status queries — implemented by the infrastructure layer.
pub trait GitStatusPort {
    /// Check if there are uncommitted changes in the given repo path.
    fn has_uncommitted_changes(&self, repo_path: &str) -> anyhow::Result<bool>;

    /// Get list of changed files (staged + unstaged).
    fn changed_files(&self, repo_path: &str) -> anyhow::Result<Vec<String>>;

    /// Get the current branch name (e.g. "main", "feat/my-feature").
    fn current_branch(&self, repo_path: &str) -> anyhow::Result<String>;

    /// Check if the path is inside a git worktree (not the main working tree).
    fn is_worktree(&self, repo_path: &str) -> bool;

    /// Check if local branch has commits not yet pushed to remote.
    fn has_unpushed_commits(&self, repo_path: &str) -> anyhow::Result<bool>;

    /// Get the repository root (absolute path) for the given working path.
    /// Returns `None` if the path is not inside any git repo.
    fn repo_root(&self, path: &str) -> Option<String>;

    /// List the directory basenames of every worktree registered in this repo's
    /// `git worktree list`. Returns just the trailing path segment of each
    /// worktree path so callers can compare against directory names inside
    /// `.claude/worktrees/`. The primary (main) worktree is included too, though
    /// callers typically only care about secondary worktrees.
    ///
    /// Returns an empty Vec on error (e.g. not a git repo, or git unavailable) —
    /// callers should treat that as "trust nothing" rather than "everything is
    /// orphaned". Used by `hygiene_reminders` to distinguish orphaned dirs (no
    /// git registry entry — truly stale) from actively-used worktrees
    /// (registered, possibly in another agent session).
    fn list_worktree_names(&self, repo_path: &str) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// Vector store port (Qdrant)
// ---------------------------------------------------------------------------

/// Port for vector database operations (Qdrant).
///
/// Abstracts all HTTP calls to Qdrant Cloud. Memory hooks use this for
/// search, upsert, scroll, payload updates. The infrastructure layer
/// handles auth, URL construction, and HTTP client lifecycle.
///
/// All methods are async — the infrastructure implementation uses reqwest.
/// Hook callers must wrap in a tokio runtime (or use `run_async`).
#[async_trait::async_trait]
pub trait VectorStorePort: Send + Sync {
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

// ---------------------------------------------------------------------------
// Filesystem port
// ---------------------------------------------------------------------------

/// Port for filesystem operations.
///
/// Abstracts all std::fs and dirs calls. Hooks use this for reading/writing
/// state files, config, metrics, and memory files. The infrastructure layer
/// delegates to real std::fs. Tests can inject a mock.
pub trait FileSystemPort: Send + Sync {
    /// Get the user's home directory.
    fn home_dir(&self) -> Option<PathBuf>;

    /// Read a file's contents as a string.
    fn read_to_string(&self, path: &Path) -> anyhow::Result<String>;

    /// Write bytes to a file (creates parent dirs if needed).
    fn write(&self, path: &Path, content: &[u8]) -> anyhow::Result<()>;

    /// Create a directory and all parent directories.
    fn create_dir_all(&self, path: &Path) -> anyhow::Result<()>;

    /// List entries in a directory (returns paths).
    fn read_dir(&self, path: &Path) -> anyhow::Result<Vec<PathBuf>>;

    /// Check if a path exists.
    fn exists(&self, path: &Path) -> bool;

    /// Check if a path is a directory.
    fn is_dir(&self, path: &Path) -> bool;

    /// Get file metadata (for mtime checks).
    fn metadata(&self, path: &Path) -> anyhow::Result<std::fs::Metadata>;

    /// Append bytes to a file (creates if needed, does not truncate).
    fn append(&self, path: &Path, content: &[u8]) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Process port
// ---------------------------------------------------------------------------

/// Port for spawning external processes.
///
/// Abstracts std::process::Command calls for binary execution and
/// fire-and-forget spawns. Used by session_init (qdrant sync, git),
/// pre_push_steel_test (git).
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
// LLM port
// ---------------------------------------------------------------------------

/// Port for free-form LLM text completion.
///
/// Wraps Anthropic / OpenRouter / etc. for hooks that need an LLM call but
/// don't fit the existing `AiClassifier` or `JudgeService` shapes (those are
/// classification- and verdict-shaped). Used by `memory_verify` for claim
/// extraction (Claude Haiku) and is generic enough for future LLM hooks.
#[async_trait::async_trait]
pub trait LlmPort: Send + Sync {
    /// Run a completion. Returns the model's text response.
    async fn complete(&self, request: LlmRequest) -> anyhow::Result<String>;
}

/// Request for an LLM completion.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// Logical model — the adapter maps to a provider-specific ID.
    pub model: LlmModel,
    /// User prompt.
    pub prompt: String,
    /// Maximum tokens in the response.
    pub max_tokens: u32,
}

/// Logical LLM model — adapter maps to provider-specific IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmModel {
    /// Fast, cheap classification / extraction.
    Haiku,
    /// Mid-tier reasoning.
    Sonnet,
    /// Heavy reasoning.
    Opus,
}

// ---------------------------------------------------------------------------
// Memory-MCP port
// ---------------------------------------------------------------------------

/// Port for calling tools on the Memory engine MCP server (`memory-mcp`).
///
/// Wraps the MCP stdio handshake + tool-call loop so hooks can talk to the
/// Memory engine without each one inlining its own subprocess transport.
/// `call_tool` is intentionally generic — every Memory engine tool reduces
/// to "send a tool name + JSON args, get JSON back", and a typed surface
/// per tool would balloon the port for no behavioural gain.
#[async_trait::async_trait]
pub trait MemoryMcpPort: Send + Sync {
    /// Call any tool on memory-mcp. Returns the parsed JSON payload from
    /// `result.structuredContent` (preferred) or `result.content[0].text`
    /// (fallback) on the MCP response. Errors when the subprocess fails to
    /// spawn, the handshake fails, the tool returns an error, or the
    /// response payload is missing.
    async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value>;
}
