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

    /// Resolve `git merge-base HEAD <base_ref>` and return the SHA, or `None`
    /// if the ref doesn't resolve / merge-base fails. Used by
    /// `pre_push_browser_test` to find the closest common ancestor against
    /// candidate base refs (`origin/main`, `@{upstream}`, etc.) so frontend-
    /// file detection scopes to the branch's own commits.
    fn merge_base(&self, repo_path: &str, base_ref: &str) -> Option<String>;

    /// Count commits in `<from>..HEAD` (exclusive `from`, inclusive HEAD).
    /// Returns `None` if the range can't be evaluated (bad ref / not a repo).
    /// Used by `pre_push_browser_test` to pick the merge-base candidate whose
    /// distance from HEAD is shortest (most-recent common ancestor).
    fn rev_list_count(&self, repo_path: &str, from: &str) -> Option<u32>;

    /// Run `git diff --name-only <range>` and return the changed file paths.
    /// `range` is the diff spec — `"HEAD"`, `"--cached"`, `"main..HEAD"`,
    /// `"<sha>..HEAD"`, etc. Returns `None` on git failure (bad ref, not a
    /// repo, etc.) so callers can distinguish "no diff" from "couldn't ask".
    ///
    /// Note: `--cached` is passed as the range string itself; the adapter
    /// runs `git diff --name-only --cached` in that case. Any string that
    /// starts with `--` is forwarded as a flag.
    fn diff_names(&self, repo_path: &str, range: &str) -> Option<Vec<String>>;

    /// Return local branch names that are fully merged into `base_ref`.
    /// Runs `git branch --merged <base_ref>` and excludes the base ref itself
    /// plus `main`/`master`/`HEAD`. Returns empty Vec on error.
    fn merged_local_branches(&self, repo_path: &str, base_ref: &str) -> Vec<String>;

    /// Return remote branch names (without `origin/` prefix) that are fully
    /// merged into `base_ref`. Runs `git branch -r --merged <base_ref>` and
    /// excludes `HEAD`, `main`, `master`, and `<base_ref>` itself.
    /// Returns empty Vec on error.
    fn merged_remote_branches(&self, repo_path: &str, base_ref: &str) -> Vec<String>;
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
    async fn upsert_points(&self, collection: &str, points: Vec<VectorPoint>)
        -> anyhow::Result<()>;

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

/// A point returned from scroll or `get_points`.
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
/// Abstracts all `std::fs` and dirs calls. Hooks use this for reading/writing
/// state files, config, metrics, and memory files. The infrastructure layer
/// delegates to real `std::fs`. Tests can inject a mock.
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

    /// Copy a file from `src` to `dst`. Required for the metrics-dir migration
    /// in `session_init` (move = copy + remove for cross-device safety).
    ///
    /// Default impl: returns Ok(()) without doing anything. Stub adapters in
    /// tests that exercise the copy path must override; the real adapter in
    /// `sentinel-infrastructure` overrides with `std::fs::copy`. Default
    /// exists so the 20+ existing test stubs don't need to change.
    fn copy(&self, _src: &Path, _dst: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// Remove a single file. No-op if the file doesn't exist; errors only on
    /// permission failures or unexpected IO errors. Used by hooks that maintain
    /// short-lived state markers (`skill_router`, `verification_gate`, `session_init`).
    ///
    /// Default impl: Ok(()). See `copy` for rationale.
    fn remove_file(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// Remove an empty directory. Errors if the directory is non-empty —
    /// callers should clear contents first. Used by `session_init` to prune
    /// the legacy `~/.claude/metrics/` directory after migrating its contents.
    ///
    /// Default impl: Ok(()). See `copy` for rationale.
    fn remove_dir(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// Resolve `path` to its canonical absolute form (follows symlinks /
    /// junctions on Windows, drops `.` and `..` components). Used by
    /// `git_hygiene` to compare worktree-edit targets against the canonical
    /// repo root, and by `phase_gate`'s symlink-escape detector.
    ///
    /// Default impl: returns the input path unchanged. The real adapter
    /// in `sentinel-infrastructure` overrides with `std::fs::canonicalize`.
    /// Stub callers that don't exercise canonicalization can rely on the
    /// no-op default.
    fn canonicalize(&self, path: &Path) -> anyhow::Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    /// Recursively remove a directory and all its contents. Used by
    /// `channel_events::cleanup_stale_sessions` to prune stale per-session
    /// event directories on `SessionStart`.
    ///
    /// Default impl: Ok(()) — non-destructive no-op so existing test stubs
    /// don't need updating. The real adapter in `sentinel-infrastructure`
    /// overrides with `std::fs::remove_dir_all`. Tests that exercise
    /// recursive removal must inject an adapter that performs the deletion.
    fn remove_dir_all(&self, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Process port
// ---------------------------------------------------------------------------

/// Port for spawning external processes.
///
/// Abstracts `std::process::Command` calls for binary execution and
/// fire-and-forget spawns. Used by `session_init` (qdrant sync, git),
/// `pre_push_browser_test` (git).
pub trait ProcessPort: Send + Sync {
    /// Run a command and capture output. Returns (`exit_success`, stdout, stderr).
    fn run(&self, command: &str, args: &[&str], cwd: Option<&str>)
        -> anyhow::Result<ProcessOutput>;

    /// Spawn a detached process (fire-and-forget). Returns immediately.
    fn spawn_detached(&self, command: &str, args: &[&str]) -> anyhow::Result<()>;
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
/// Wraps Anthropic / `OpenRouter` / etc. for hooks that need an LLM call but
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
// Environment port
// ---------------------------------------------------------------------------

/// Port for reading process environment variables.
///
/// Abstracts `std::env::var` / `std::env::var_os` so hooks don't reach for
/// the global env directly. Tests inject a `StubEnv` with a fixed map; the
/// real adapter delegates to `std::env`. Used for the session-id idiom
/// (`CLAUDE_SESSION_ID` → fallback `SESSION_ID`), the `SENTINEL_AUTOPILOT`
/// flag, and ntfy/`CLAUDE_ENV_FILE` config reads.
pub trait EnvPort: Send + Sync {
    /// Read a UTF-8 environment variable. Returns `None` if absent or the
    /// value is not valid UTF-8 (matches `std::env::var(...).ok()`).
    fn var(&self, key: &str) -> Option<String>;

    /// Read an environment variable as `OsString` (handles non-UTF-8 values
    /// like Windows HOME paths). Returns `None` if absent.
    fn var_os(&self, key: &str) -> Option<std::ffi::OsString>;
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

// ---------------------------------------------------------------------------
// Reversibility classifier port (A6)
// ---------------------------------------------------------------------------

/// Port for classifying a tool call by its reversibility class (A6 design,
/// `docs/a6-reversibility-graded-tripwires.md`). The single shared
/// blast-radius axis every gate in sentinel consults.
///
/// Consumers (specified across the A-tier design docs):
/// - `tool_usage_gate` — replaces the binary `in_scope` decision with the
///   class so trivially-reversible writes (memory notes, plan files) skip
///   the four-check stack.
/// - `dry_run_then_commit` (A3) — fires only when class is at least
///   `Irreversible`; the auditor-seat selection (A2) gains stricter
///   reasoning requirements for `Catastrophic`.
/// - `ba_critique` (BA5) — derives BA artifact-class (Routine / Substantial
///   / Catastrophic) from the publish-tool class.
/// - `provenance_validate` (BA1+BA3) — tightens citation freshness window
///   (1h vs 24h) for `Catastrophic` outputs.
/// - `requirements_traceability_gate` (BA1+BA3) — requires coverage for
///   outputs at class `Irreversible` or above.
///
/// The Phase 3 adapter (in `sentinel-infrastructure`) evaluates four
/// layers in order: built-in tool defaults → per-MCP-tool defaults →
/// per-input contextual rules (Bash patterns) → operator overrides
/// (`config/reversibility.toml`). The trait itself is layer-agnostic; the
/// adapter is free to use whatever evaluation scheme satisfies the
/// contract.
pub trait ReversibilityClassifierPort: Send + Sync {
    /// Classify a tool call by reversibility. Implementations must be
    /// deterministic for the same `(tool_name, tool_input)` pair given the
    /// same configuration — the router relies on this for explainable
    /// decisions. Unknown tools should default conservatively to
    /// `ReversibilityClass::Irreversible` so the system fails toward
    /// safety.
    fn classify(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> crate::ReversibilityClass;
}

// ---------------------------------------------------------------------------
// Auditor port (A3)
// ---------------------------------------------------------------------------

/// Port for scoring a dry-run artifact via a separate-model-family auditor.
///
/// A3 design (`docs/a3-dry-run-then-commit.md`): the hook constructs a
/// [`DryRunRequest`](crate::dry_run::DryRunRequest) when an action's
/// reversibility class is `Irreversible` or `Catastrophic`, routes it
/// through this port, and uses the returned
/// [`AuditorVerdict`](crate::dry_run::AuditorVerdict) to decide whether
/// to commit, block, or escalate to human review.
///
/// The vendor-class separation contract — auditor must be different model
/// family than the acting agent — is the responsibility of the *adapter
/// selector* (per A2's `CapabilityRouterPort`), not this trait. Adapter
/// implementations focus on a single vendor; the selector picks the
/// appropriate one for each call.
///
/// Implementations must be **deterministic at the configuration level**
/// (same configured model + temperature + prompts → reproducible verdicts
/// for the same dry-run, modulo LLM-side variation) so proof-chain
/// re-verification reads cleanly.
pub trait AuditorPort: Send + Sync {
    /// Score a dry-run artifact. On success returns a structured
    /// [`AuditorVerdict`](crate::dry_run::AuditorVerdict). On failure
    /// returns an [`AuditorError`](crate::dry_run::AuditorError); the
    /// hook treats every error per the catastrophic-vs-irreversible
    /// policy in the `AuditorError` doc comment.
    fn score(
        &self,
        dry_run: &crate::dry_run::DryRunRequest,
    ) -> Result<crate::dry_run::AuditorVerdict, crate::dry_run::AuditorError>;
}

// ---------------------------------------------------------------------------
// CapabilityRouterPort + AppraisalStorePort (A2)
// ---------------------------------------------------------------------------

/// Why the router couldn't pick an agent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RoutingError {
    /// No registered agent satisfies the `required` capabilities (or
    /// every candidate matched a `forbidden` capability). Carries the
    /// nearest-miss diagnostics so the operator can fix profiles or
    /// relax the requirement.
    NoAgentSatisfies(Vec<crate::agent_routing::UnsatisfiedRequirement>),
    /// Operator-side misconfiguration — malformed profile, contradictory
    /// tie-breaker policy, etc. Surfaced at startup; sentinel refuses
    /// to dispatch until corrected.
    Configuration(String),
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoAgentSatisfies(reqs) => {
                write!(
                    f,
                    "no registered agent satisfies the requirement ({} unsatisfied capability/agent pairs)",
                    reqs.len()
                )
            }
            Self::Configuration(msg) => {
                write!(f, "router configuration error: {msg}")
            }
        }
    }
}

impl std::error::Error for RoutingError {}

/// A2 — substrate dispatch primitive.
///
/// Every "which agent / which model for this work?" decision in sentinel
/// goes through [`Self::route`]. Replaces hardcoded vendor pairings
/// (A3's `select_auditor_for(acting_agent)`) and per-hook static config
/// (R1's retired role-persona pipelines) with a single capability-graph
/// substrate.
///
/// The router is **deterministic at the configuration level**: same
/// requirement + same registered profiles + same appraisal data
/// produces the same `chosen` agent. Tie-breakers are explicit
/// ([`crate::agent_routing::TieBreaker`]) so the operator can reason
/// about choices.
///
/// Implementations must be `Send + Sync` because the router is shared
/// across hook invocations within a session and is consulted from the
/// hook engine (synchronous, single-thread) plus the daemon (async,
/// multi-thread).
pub trait CapabilityRouterPort: Send + Sync {
    /// Pick the best-fit agent for the requirement. Returns
    /// [`RoutingError::NoAgentSatisfies`] when no candidate clears the
    /// `required` + `forbidden` filters.
    fn route(
        &self,
        requirement: &crate::capability::CapabilityRequirement,
    ) -> Result<crate::capability::AgentId, RoutingError>;

    /// Return every agent that satisfies the `required` + `forbidden`
    /// filters, *before* tie-breakers run. Used by `routing explain`
    /// and by callers that want to fan out across all candidates
    /// (debate-style audits, multi-vote critiques).
    fn candidates(
        &self,
        requirement: &crate::capability::CapabilityRequirement,
    ) -> Vec<crate::capability::AgentId>;

    /// Return the full decision tree — what was considered, what was
    /// eliminated and why, which tie-breakers fired. Operator-facing
    /// tooling renders this for "why did the router pick X instead
    /// of Y?" questions.
    fn explain(
        &self,
        requirement: &crate::capability::CapabilityRequirement,
    ) -> crate::agent_routing::RoutingExplanation;
}

/// A2 — appraisal store.
///
/// Per-agent / per-requirement outcome history. The router reads
/// aggregates from this port (tie-breaker step 3); hooks record new
/// outcomes after work completes.
///
/// **R5 quarantine boundary**: appraisal data is *dispatch input* only.
/// It must never reach the agents themselves as training feedback. The
/// distinction is load-bearing — using past success as a reward signal
/// is exactly the deception-amplifier loop R5 prohibits.
pub trait AppraisalStorePort: Send + Sync {
    /// Persist a single outcome record. Implementations are expected
    /// to be best-effort — failures should not block the dispatching
    /// hook, but should be logged at warn level.
    fn record(&self, record: crate::agent_routing::AppraisalRecord);

    /// Aggregate stats for `(agent, requirement_signature)` over the
    /// given window. Returns [`crate::agent_routing::AggregateStats::empty`]
    /// when no records exist for the bucket (so the router falls through
    /// to the next tie-breaker rather than erroring).
    fn aggregate(
        &self,
        agent_id: &crate::capability::AgentId,
        signature: &crate::agent_routing::RequirementSignature,
        window: crate::agent_routing::AppraisalWindow,
    ) -> crate::agent_routing::AggregateStats;
}

// ---------------------------------------------------------------------------
// ProvenancePort + RequirementMatrixPort (BA1 + BA3)
// ---------------------------------------------------------------------------

/// Errors `ProvenancePort` can surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProvenanceError {
    /// The audit chain backing store was unreachable (filesystem
    /// error, locked file, etc.). Hooks treat as a soft-warn —
    /// `provenance_validate` can't validate without history, but
    /// the operator should see why.
    StoreUnavailable(String),
    /// Backing-store data was malformed (corrupt JSONL line,
    /// schema mismatch). Carries the offending detail.
    Malformed(String),
}

impl std::fmt::Display for ProvenanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreUnavailable(msg) => {
                write!(f, "provenance audit store unavailable: {msg}")
            }
            Self::Malformed(msg) => write!(f, "provenance record malformed: {msg}"),
        }
    }
}

impl std::error::Error for ProvenanceError {}

/// BA1 — citation-provenance audit chain access.
///
/// Per `docs/ba1-ba3-sentinel-enforcement.md` §9. The
/// `provenance_validate` hook (future phase) reads
/// [`RetrievalRecord`](crate::ba::RetrievalRecord)s through this
/// port to validate that each cited
/// [`ArtifactReference`](crate::ba::ArtifactReference) in a
/// BA-orchestrator output corresponds to a real connector retrieval.
///
/// Returns the full history for an `artifact_id` so the caller can
/// pick the most-recent retrieval, scan within a freshness window,
/// or detect repeated retrievals (some workflows pull an artifact
/// multiple times within a session).
pub trait ProvenancePort: Send + Sync {
    /// Query every retrieval record for `artifact_id` across the
    /// configured audit window (operator-set; typically last 24h).
    /// Empty `Vec` means the artifact has never been retrieved
    /// through any registered connector in the window — the BA1
    /// `Existence` check fails on this signal.
    fn query_artifact_history(
        &self,
        artifact_id: &str,
    ) -> Result<Vec<crate::ba::RetrievalRecord>, ProvenanceError>;
}

/// BA1 — citation-provenance audit chain WRITE side.
///
/// The `audit_extract` hook (`PostToolUse`) emits a
/// [`RetrievalRecord`](crate::ba::RetrievalRecord) every time a
/// documented MCP connector successfully retrieves an artifact.
/// Phase 4's JSONL adapter will implement BOTH this trait and
/// [`ProvenancePort`] over the same backing file — the read +
/// write paths share storage so callers don't see a race window
/// between "`audit_extract` emitted" and "`provenance_validate` sees
/// the record."
///
/// Best-effort persistence: failures must surface as
/// [`ProvenanceError`] but consumer hooks should NOT block on
/// write failures — appraisal-style observability (the validate
/// hook handles the missing-record case as a Block-class finding,
/// which is the right behavior whether the record is missing
/// because the connector wasn't called OR because the write
/// failed).
pub trait ProvenanceWritePort: Send + Sync {
    /// Persist a single retrieval record. Idempotent: if the
    /// `(artifact_id, content_hash, retrieved_at)` tuple already
    /// exists, the adapter MAY skip the write but MUST still return
    /// `Ok(())`. Implementations are free to write through (the
    /// JSONL adapter appends every call; consumers de-dupe at
    /// read time).
    fn record(&self, record: crate::ba::RetrievalRecord) -> Result<(), ProvenanceError>;
}

/// Errors `RequirementMatrixPort` can surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RequirementMatrixError {
    /// The matrix endpoint was unreachable. Per spec §8.3 the
    /// adapter has a `last_known_good` fallback — this error is
    /// reserved for the case where no snapshot is available at
    /// all (fresh install / never-fetched orchestration).
    MatrixUnavailable(String),
    /// The orchestration is registered but no row matches the
    /// supplied `matrix_row_id`. Distinct from `Ok(None)`: this
    /// error means the orchestration itself isn't tracked.
    UnknownOrchestration(String),
    /// Schema mismatch / corrupt response from the matrix.
    Malformed(String),
}

impl std::fmt::Display for RequirementMatrixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MatrixUnavailable(msg) => write!(f, "requirement matrix unavailable: {msg}"),
            Self::UnknownOrchestration(id) => {
                write!(f, "requirement matrix has no orchestration {id:?}")
            }
            Self::Malformed(msg) => write!(f, "requirement matrix payload malformed: {msg}"),
        }
    }
}

impl std::error::Error for RequirementMatrixError {}

/// BA3 — requirements-traceability matrix access.
///
/// Per `docs/ba1-ba3-sentinel-enforcement.md` §9. The
/// `requirements_traceability_gate` hook (future phase) queries
/// individual matrix rows through this port to validate that each
/// cited [`RequirementRef`](crate::ba::RequirementRef) corresponds
/// to a live row in the orchestrator's matrix.
///
/// `Ok(None)` means the row was queried successfully but doesn't
/// exist — that's a BA3 `Existence` failure, not a port error.
/// `Err(UnknownOrchestration)` is reserved for the orchestrator-
/// level missing case (a typo in the citation's `orchestration_id`).
pub trait RequirementMatrixPort: Send + Sync {
    /// Look up a single row by `(orchestration_id, matrix_row_id)`.
    /// Returns the live row content so the caller can validate the
    /// citation's `content_hash` against the current matrix state
    /// (BA3 `Hash` finding).
    fn query_requirement(
        &self,
        orchestration_id: &str,
        matrix_row_id: &str,
    ) -> Result<Option<crate::ba::RequirementRef>, RequirementMatrixError>;
}

// ---------------------------------------------------------------------------
// EvalScorerPort + EvalRunStorePort (A12 Phase 3b)
// ---------------------------------------------------------------------------

/// Errors `EvalScorerPort` can surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EvalScorerError {
    /// Backend call (LLM API, sidecar process, etc.) failed —
    /// network, timeout, rate limit, malformed upstream response.
    /// Recorded on the [`EvalCaseResult`](crate::eval::EvalCaseResult)
    /// `error` field; the run continues with the next case.
    Backend(String),
    /// Scorer produced output the adapter couldn't decode into
    /// [`EvalAxisScore`](crate::eval::EvalAxisScore)s. Distinct from
    /// [`Self::Backend`]: this means the LLM returned a response but
    /// the response didn't conform to the expected schema.
    Malformed(String),
    /// Operator-side misconfiguration — missing model handle, bad
    /// API key, unknown judge profile. Surfaces at startup or on
    /// the first scoring call; not a per-case transient.
    Configuration(String),
}

impl std::fmt::Display for EvalScorerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "eval scorer backend error: {msg}"),
            Self::Malformed(msg) => write!(f, "eval scorer output malformed: {msg}"),
            Self::Configuration(msg) => write!(f, "eval scorer configuration error: {msg}"),
        }
    }
}

impl std::error::Error for EvalScorerError {}

/// A12 — score a candidate output against an [`EvalCase`](crate::eval::EvalCase).
///
/// Per `docs/a12-external-benchmarks.md` §3.5. The benchmark runner
/// (Phase 3c) calls `score` once per `(case, candidate_output)` pair
/// after dispatching the case through the A2 capability router. The
/// returned [`EvalScore`](crate::eval::EvalScore) carries per-axis
/// raw scores + the rubric-weighted composite, ready to embed in
/// the [`EvalCaseResult`](crate::eval::EvalCaseResult).
///
/// Implementations are typically LLM-as-judge (Phase 3d ships a Rig-
/// based adapter using the A2 router to pick the judge model), but
/// nothing in this trait constrains the strategy — a deterministic
/// regex scorer, a human-in-the-loop tool, or a hybrid would all
/// satisfy the contract.
///
/// **Read the case's [`ScoringRubric`](crate::eval::ScoringRubric)**:
/// the scorer is responsible for honoring per-case weight overrides
/// (operators may override the BA-default rubric per archetype). The
/// trait passes the full [`EvalCase`](crate::eval::EvalCase) so the
/// adapter has access to the rubric, the gold artifact, gold
/// outcomes, and the stakeholder brief without the application
/// layer having to plumb them separately.
///
/// `Send + Sync` because the scorer is shared across runner
/// iterations (one scorer instance per run); implementations that
/// rely on internal mutex-guarded state (HTTP connection pools,
/// rate-limit counters) are expected to manage that internally.
pub trait EvalScorerPort: Send + Sync {
    /// Score a single candidate output against the case's rubric.
    /// `run_id` is echoed into the returned
    /// [`EvalScore`](crate::eval::EvalScore) for downstream
    /// aggregation; the scorer itself is stateless across runs.
    fn score(
        &self,
        case: &crate::eval::EvalCase,
        candidate_output: &str,
        run_id: &crate::eval::EvalRunId,
    ) -> Result<crate::eval::EvalScore, EvalScorerError>;
}

/// Errors `EvalRunStorePort` can surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EvalRunStoreError {
    /// Backing store (filesystem, database) unreachable. Per-run
    /// persistence failures are surfaced to the runner so the
    /// operator sees them; the in-memory
    /// [`EvalRunResult`](crate::eval::EvalRunResult) is still
    /// returned from the use case so the CLI can render the data
    /// even when persistence fails.
    StoreUnavailable(String),
    /// Stored payload was malformed (corrupt file, schema drift).
    /// Distinct from `Ok(None)` for `load`: `Malformed` means the
    /// record exists but is unreadable.
    Malformed(String),
}

impl std::fmt::Display for EvalRunStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreUnavailable(msg) => write!(f, "eval run store unavailable: {msg}"),
            Self::Malformed(msg) => write!(f, "eval run record malformed: {msg}"),
        }
    }
}

impl std::error::Error for EvalRunStoreError {}

/// A12 — persist + retrieve [`EvalRunResult`](crate::eval::EvalRunResult)
/// records.
///
/// Per spec §3.5. Phase 3d ships the JSONL implementation under
/// `~/.claude/sentinel/state/ba-corpus/runs/{run_id}.json` (one
/// JSON file per run — runs are append-only, never edited). The
/// CLI (Phase 3e) reads from this port to render `sentinel eval
/// show <run-id>` and `sentinel eval list-runs`.
///
/// `save` is write-once-per-run: callers persist the run after the
/// runner finishes all cases. There's no per-case write API on this
/// port — the runner accumulates in memory and persists the full
/// result at the end. This keeps the storage layer simple and the
/// run record atomically consistent.
pub trait EvalRunStorePort: Send + Sync {
    /// Persist a complete run. Idempotent: if a record with the
    /// same `run_id` already exists, the implementation MAY
    /// overwrite (e.g., a re-run replaces the prior attempt) or
    /// reject; the JSONL adapter overwrites since runs are
    /// produced by an explicit operator action and the typical
    /// case is "retry after a backend fix".
    fn save(&self, run: &crate::eval::EvalRunResult) -> Result<(), EvalRunStoreError>;

    /// Load a single run by id. Returns `Ok(None)` when the run
    /// doesn't exist (vs. [`EvalRunStoreError::Malformed`] when it
    /// exists but can't be parsed).
    fn load(
        &self,
        run_id: &crate::eval::EvalRunId,
    ) -> Result<Option<crate::eval::EvalRunResult>, EvalRunStoreError>;

    /// Enumerate every persisted run id. Used by `sentinel eval
    /// list-runs`. Ordering is implementation-defined — operators
    /// rendering the list typically sort by `completed_at` after
    /// hydrating via [`Self::load`].
    fn list_run_ids(&self) -> Result<Vec<crate::eval::EvalRunId>, EvalRunStoreError>;
}

// ---------------------------------------------------------------------------
// Tests — BA1 + BA3 port surface
// ---------------------------------------------------------------------------

#[cfg(test)]
mod ba_port_tests {
    use super::*;

    #[test]
    fn provenance_error_display_names_each_variant() {
        let unavailable =
            ProvenanceError::StoreUnavailable("disk full".to_string()).to_string();
        assert!(unavailable.contains("unavailable"));
        assert!(unavailable.contains("disk full"));

        let malformed = ProvenanceError::Malformed("bad json".to_string()).to_string();
        assert!(malformed.contains("malformed"));
        assert!(malformed.contains("bad json"));
    }

    #[test]
    fn requirement_matrix_error_display_names_each_variant() {
        let unavailable =
            RequirementMatrixError::MatrixUnavailable("timeout".to_string()).to_string();
        assert!(unavailable.contains("unavailable"));
        let unknown =
            RequirementMatrixError::UnknownOrchestration("ghost-id".to_string()).to_string();
        assert!(unknown.contains("ghost-id"));
        let malformed = RequirementMatrixError::Malformed("schema".to_string()).to_string();
        assert!(malformed.contains("malformed"));
    }

    #[test]
    fn errors_implement_std_error_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<ProvenanceError>();
        assert_error::<RequirementMatrixError>();
    }

    #[test]
    fn errors_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProvenanceError>();
        assert_send_sync::<RequirementMatrixError>();
    }

    #[test]
    fn ports_can_be_used_through_trait_objects() {
        // Sanity check: the traits can be wrapped in `Box<dyn …>` —
        // confirms the `Send + Sync` supertrait bounds and the
        // method signatures are object-safe.
        fn _take_provenance(_: Box<dyn ProvenancePort>) {}
        fn _take_matrix(_: Box<dyn RequirementMatrixPort>) {}
    }

    #[test]
    fn errors_roundtrip_through_json() {
        let original = ProvenanceError::StoreUnavailable("io error".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ProvenanceError = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);

        let original = RequirementMatrixError::UnknownOrchestration("xyz".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: RequirementMatrixError = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}

// ---------------------------------------------------------------------------
// SpecChallengeScorerPort + SpecChallengeStorePort (A13 Phase 2)
// ---------------------------------------------------------------------------

/// Per-category semantic-quality score from the
/// [`SpecChallengeScorerPort`]. Range `[0.0, 1.0]`; clamped at
/// construction.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpecChallengeScore {
    pub assumptions: f32,
    pub gaps: f32,
    pub ambiguities: f32,
    pub alternatives_considered: f32,
    pub constraints_not_satisfied: f32,
}

impl SpecChallengeScore {
    /// Construct, clamping every axis to `[0.0, 1.0]`.
    #[must_use]
    pub fn new(
        assumptions: f32,
        gaps: f32,
        ambiguities: f32,
        alternatives_considered: f32,
        constraints_not_satisfied: f32,
    ) -> Self {
        Self {
            assumptions: assumptions.clamp(0.0, 1.0),
            gaps: gaps.clamp(0.0, 1.0),
            ambiguities: ambiguities.clamp(0.0, 1.0),
            alternatives_considered: alternatives_considered.clamp(0.0, 1.0),
            constraints_not_satisfied: constraints_not_satisfied.clamp(0.0, 1.0),
        }
    }

    /// True when **every** axis meets or exceeds `threshold`. The
    /// Catastrophic-class gate requires this; lower classes use
    /// the deterministic completeness check alone.
    #[must_use]
    pub fn all_axes_above(&self, threshold: f32) -> bool {
        self.assumptions >= threshold
            && self.gaps >= threshold
            && self.ambiguities >= threshold
            && self.alternatives_considered >= threshold
            && self.constraints_not_satisfied >= threshold
    }

    /// Minimum axis value across all five. Useful for ranking + for
    /// rendering "weakest axis" in operator dashboards.
    #[must_use]
    pub fn min_axis(&self) -> f32 {
        [
            self.assumptions,
            self.gaps,
            self.ambiguities,
            self.alternatives_considered,
            self.constraints_not_satisfied,
        ]
        .into_iter()
        .fold(f32::INFINITY, f32::min)
    }
}

/// Errors `SpecChallengeScorerPort` can surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpecChallengeScorerError {
    /// Backend (LLM / sidecar) failed transiently.
    Backend(String),
    /// Scorer returned a response the adapter couldn't decode into
    /// [`SpecChallengeScore`].
    Malformed(String),
    /// Operator-side misconfiguration (missing model handle, bad
    /// API key). Surfaces at startup or first call.
    Configuration(String),
}

impl std::fmt::Display for SpecChallengeScorerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(msg) => write!(f, "spec challenge scorer backend error: {msg}"),
            Self::Malformed(msg) => write!(f, "spec challenge scorer output malformed: {msg}"),
            Self::Configuration(msg) => {
                write!(f, "spec challenge scorer configuration error: {msg}")
            }
        }
    }
}

impl std::error::Error for SpecChallengeScorerError {}

/// A13 — semantic-quality score for a [`SpecChallenge`].
///
/// Per `docs/a13-spec-challenge.md` §3, **Catastrophic-class** work
/// requires both deterministic completeness AND every axis of this
/// scorer ≥ operator-configured threshold. Irreversible and below
/// pass on completeness alone — this port is *not* consulted for
/// non-Catastrophic gates, which keeps the auditor-call budget
/// bounded.
///
/// Implementations are typically LLM-as-judge (a separate-model-
/// family challenge agent per the spec). Sync trait to match
/// [`AuditorPort`] / [`EvalScorerPort`]; adapters bridge async
/// backends via a sidecar runtime.
pub trait SpecChallengeScorerPort: Send + Sync {
    fn score(
        &self,
        challenge: &crate::spec_challenge::SpecChallenge,
    ) -> Result<SpecChallengeScore, SpecChallengeScorerError>;
}

/// Errors `SpecChallengeStorePort` can surface.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpecChallengeStoreError {
    StoreUnavailable(String),
    Malformed(String),
}

impl std::fmt::Display for SpecChallengeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StoreUnavailable(msg) => {
                write!(f, "spec challenge store unavailable: {msg}")
            }
            Self::Malformed(msg) => write!(f, "spec challenge record malformed: {msg}"),
        }
    }
}

impl std::error::Error for SpecChallengeStoreError {}

/// A13 — persist + retrieve [`SpecChallenge`] artifacts.
///
/// The hook layer (Phase 3) saves every emitted challenge so the
/// proof chain can re-verify "what did the agent challenge before
/// it acted?" later. One file per [`WorkId`](crate::spec_challenge::WorkId)
/// per spec §6; re-emissions for the same `work_id` overwrite
/// (the typical case is "agent re-attempted after the first
/// challenge was rejected").
pub trait SpecChallengeStorePort: Send + Sync {
    fn save(
        &self,
        challenge: &crate::spec_challenge::SpecChallenge,
    ) -> Result<(), SpecChallengeStoreError>;

    fn load(
        &self,
        work_id: &crate::spec_challenge::WorkId,
    ) -> Result<Option<crate::spec_challenge::SpecChallenge>, SpecChallengeStoreError>;
}

// ---------------------------------------------------------------------------
// Tests — A12 Phase 3b port surface
// ---------------------------------------------------------------------------

#[cfg(test)]
mod eval_port_tests {
    use super::*;

    #[test]
    fn scorer_error_display_names_each_variant() {
        assert!(EvalScorerError::Backend("timeout".to_string())
            .to_string()
            .contains("timeout"));
        assert!(EvalScorerError::Malformed("bad axes".to_string())
            .to_string()
            .contains("malformed"));
        assert!(EvalScorerError::Configuration("no model".to_string())
            .to_string()
            .contains("configuration"));
    }

    #[test]
    fn run_store_error_display_names_each_variant() {
        assert!(EvalRunStoreError::StoreUnavailable("disk".to_string())
            .to_string()
            .contains("unavailable"));
        assert!(EvalRunStoreError::Malformed("schema".to_string())
            .to_string()
            .contains("malformed"));
    }

    #[test]
    fn errors_implement_std_error_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<EvalScorerError>();
        assert_error::<EvalRunStoreError>();
    }

    #[test]
    fn errors_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvalScorerError>();
        assert_send_sync::<EvalRunStoreError>();
    }

    #[test]
    fn ports_can_be_used_through_trait_objects() {
        fn _take_scorer(_: Box<dyn EvalScorerPort>) {}
        fn _take_run_store(_: Box<dyn EvalRunStorePort>) {}
    }

    #[test]
    fn errors_roundtrip_through_json() {
        let original = EvalScorerError::Backend("rate limited".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EvalScorerError = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);

        let original = EvalRunStoreError::Malformed("corrupt".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EvalRunStoreError = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}

// ---------------------------------------------------------------------------
// Tests — A13 Phase 2 port surface
// ---------------------------------------------------------------------------

#[cfg(test)]
mod spec_challenge_port_tests {
    use super::*;

    #[test]
    fn spec_challenge_score_clamps_axes_to_range() {
        let s = SpecChallengeScore::new(1.7, -0.3, 0.5, 0.5, 0.5);
        assert!((s.assumptions - 1.0).abs() < f32::EPSILON);
        assert!((s.gaps - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn all_axes_above_true_when_uniform_at_threshold() {
        let s = SpecChallengeScore::new(0.7, 0.7, 0.7, 0.7, 0.7);
        assert!(s.all_axes_above(0.7));
        assert!(s.all_axes_above(0.5));
        assert!(!s.all_axes_above(0.8));
    }

    #[test]
    fn all_axes_above_false_when_any_axis_below() {
        let s = SpecChallengeScore::new(0.9, 0.9, 0.9, 0.4, 0.9);
        assert!(!s.all_axes_above(0.5));
    }

    #[test]
    fn min_axis_returns_smallest() {
        let s = SpecChallengeScore::new(0.9, 0.4, 0.8, 0.7, 0.6);
        assert!((s.min_axis() - 0.4).abs() < f32::EPSILON);
    }

    #[test]
    fn scorer_error_display_names_each_variant() {
        assert!(SpecChallengeScorerError::Backend("timeout".to_string())
            .to_string()
            .contains("timeout"));
        assert!(SpecChallengeScorerError::Malformed("bad axes".to_string())
            .to_string()
            .contains("malformed"));
        assert!(SpecChallengeScorerError::Configuration("no model".to_string())
            .to_string()
            .contains("configuration"));
    }

    #[test]
    fn store_error_display_names_each_variant() {
        assert!(SpecChallengeStoreError::StoreUnavailable("disk".to_string())
            .to_string()
            .contains("unavailable"));
        assert!(SpecChallengeStoreError::Malformed("schema".to_string())
            .to_string()
            .contains("malformed"));
    }

    #[test]
    fn errors_implement_std_error_error() {
        fn assert_error<E: std::error::Error>() {}
        assert_error::<SpecChallengeScorerError>();
        assert_error::<SpecChallengeStoreError>();
    }

    #[test]
    fn errors_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SpecChallengeScorerError>();
        assert_send_sync::<SpecChallengeStoreError>();
        assert_send_sync::<SpecChallengeScore>();
    }

    #[test]
    fn ports_can_be_used_through_trait_objects() {
        fn _take_scorer(_: Box<dyn SpecChallengeScorerPort>) {}
        fn _take_store(_: Box<dyn SpecChallengeStorePort>) {}
    }

    #[test]
    fn errors_roundtrip_through_json() {
        let original = SpecChallengeScorerError::Backend("rate limited".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SpecChallengeScorerError = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);

        let original = SpecChallengeStoreError::Malformed("corrupt".to_string());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SpecChallengeStoreError = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}
