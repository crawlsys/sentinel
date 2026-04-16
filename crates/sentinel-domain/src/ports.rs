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
