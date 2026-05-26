//! Sentinel — Proof-of-Work Hook Engine + MCP Server
//!
//! Usage:
//!   sentinel daemon     — Start MCP server + hook listener + dashboard API
//!   sentinel hook       — Thin client, forwards to daemon (or standalone)
//!   sentinel verify     — Verify a session's proof chain
//!   sentinel mcp        — MCP server over stdio (Claude Code connects here)
//!   sentinel scan       — Scan marketplace, output JSON snapshot
//!   sentinel stats      — Hook execution statistics

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod api;
mod break_cmd;
mod cache_cmd;
mod claude_md_cmd;
mod cleanup_cmd;
mod compress_cmd;
mod config_cmd;
mod ba_cmd;
mod cost_per_point_cmd;
mod daemon_cmd;
mod deploy_freq_cmd;
mod eval_cmd;
mod federation_cmd;
mod manifest_cmd;
mod policy_cmd;
mod hook_cmd;
mod init_cmd;
mod legatus_cmd;
mod mcp_cmd;
mod pr_review_cmd;
mod project_cmd;
mod resign_cmd;
mod roi_cmd;
mod rotate_key_cmd;
mod scan_cmd;
mod schema_validator;
mod sla_cmd;
mod stage_cmd;
mod stats_cmd;
mod browser_test_cmd;
mod tokens_cmd;
mod verify_cmd;

/// Sentinel — Proof-of-Work for AI Skill Execution
#[derive(Parser)]
#[command(name = "sentinel", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the sentinel daemon (MCP server + hooks + dashboard API)
    Daemon {
        /// Dashboard API port
        #[arg(long, default_value = "3001")]
        port: u16,

        /// Optional consulate URL — when set, the daemon hosts a
        /// long-running legatus WS connection alongside the
        /// dashboard API, and exposes `POST /legatus/escalate` +
        /// `GET /legatus/inbox/next` for hook clients to use.
        /// Without it, daemon runs with no legatus (pre-B
        /// behavior).
        #[arg(long, value_name = "URL")]
        legatus_consulate_url: Option<String>,

        /// Additional consulate URL(s) to try after
        /// `--legatus-consulate-url` fails on the current attempt.
        /// Repeatable; URLs are tried in order. Empty by default.
        /// The reconnect wrapper restarts every attempt from the
        /// primary URL — failover order is not persisted across
        /// attempts so a transient primary outage doesn't
        /// permanently demote the primary.
        #[arg(long = "legatus-consulate-failover-url", value_name = "URL", action = clap::ArgAction::Append)]
        legatus_consulate_failover_urls: Vec<String>,

        /// 32-byte bootstrap secret as 64 hex chars. Required
        /// when `--legatus-consulate-url` is set.
        #[arg(long, value_name = "HEX64", env = "CONSULATE_BOOTSTRAP_SECRET", hide_env_values = true)]
        legatus_bootstrap_secret: Option<String>,

        /// Session-name hint sent in the legatus registration.
        #[arg(long, default_value = "sentinel")]
        legatus_suggested_name: String,

        /// Working directory the legatus's session is anchored
        /// to (default: daemon's cwd).
        #[arg(long)]
        legatus_working_dir: Option<String>,

        /// Heartbeat interval in seconds for the hosted legatus.
        #[arg(long, default_value_t = 20)]
        legatus_heartbeat_secs: u64,

        /// Witness verification mode for inbound `CatastrophicAck`
        /// messages.
        ///
        /// - `none` (default): no verifier installed; the daemon
        ///   trusts every ack on receipt. Matches the v0.1
        ///   daemon-local trust model.
        /// - `in-memory`: wraps an `InMemoryPraefectusClient`.
        ///   Dev / demo mode -- exercises the verification surface
        ///   end-to-end without a real Praefectus.
        /// - `http`: wraps an `HttpPraefectusClient` pointing at the
        ///   operator's reachable Praefectus. Requires
        ///   --legatus-praefectus-url + --legatus-praefectus-token
        ///   (or `LEGATUS_PRAEFECTUS_TOKEN` in env). Production
        ///   cryptographic verification path.
        #[arg(long, default_value = "none", value_parser = ["none", "in-memory", "http"])]
        legatus_witness_verify: String,

        /// Praefectus HTTP endpoint base URL. Required when
        /// --legatus-witness-verify=http.
        #[arg(long)]
        legatus_praefectus_url: Option<String>,

        /// Bearer token for the Praefectus HTTP endpoint. Reads
        /// `LEGATUS_PRAEFECTUS_TOKEN` from env so it's not exposed
        /// in process listings. Required when
        /// --legatus-witness-verify=http.
        #[arg(long, env = "LEGATUS_PRAEFECTUS_TOKEN", hide_env_values = true)]
        legatus_praefectus_token: Option<String>,

        /// Single-operator binding scaffold (v0.1). When set, the
        /// daemon logs the binding at startup so operators can
        /// confirm the daemon is bound to them. Multi-operator
        /// routing (per-session operator lookup, identity flowing
        /// through `RegisterSession` metadata) is consul-side
        /// coordination work; for now this is a declarative
        /// breadcrumb.
        #[arg(long)]
        legatus_operator_id: Option<uuid::Uuid>,
    },

    /// Stop a running sentinel daemon by reading its PID file from
    /// `~/.claude/sentinel/daemon-pid` and sending SIGTERM. Cleans
    /// up the PID + daemon-token files on success. Unix-only today
    /// (Windows lacks the same SIGTERM semantics).
    Stop {
        /// Wait up to N seconds for the daemon to exit cleanly
        /// before returning. 0 = signal and return immediately.
        #[arg(long, default_value_t = 5)]
        wait_secs: u64,
    },

    /// Process a hook event (thin client → daemon, or standalone)
    Hook {
        /// Hook event type
        #[arg(long)]
        event: String,

        /// Tool name matcher (for PreToolUse/PostToolUse)
        #[arg(long)]
        matcher: Option<String>,

        /// Run standalone (without daemon)
        #[arg(long)]
        standalone: bool,
    },

    /// Internal hook worker invoked by `sentinel hook`
    #[command(hide = true)]
    HookInternal {
        /// Hook event type
        #[arg(long)]
        event: String,

        /// Tool name matcher (for PreToolUse/PostToolUse)
        #[arg(long)]
        matcher: Option<String>,

        /// Preserve the old direct execution path for debugging
        #[arg(long)]
        standalone: bool,
    },

    /// Verify a session's proof chain
    Verify {
        /// Session ID to verify
        #[arg(long)]
        session: String,
    },

    /// Federation tooling — Apollo-style supergraph composition checks (M2.4)
    Federation {
        #[command(subcommand)]
        action: FederationAction,
    },

    /// Signed step-config manifests (M2.13). Write or verify a
    /// `manifest.toml` alongside `<config_dir>/steps/*.toml`. Supports
    /// hash-only mode (bit-rot protection) and Ed25519-signed mode
    /// (cryptographic authenticity).
    Manifest {
        #[command(subcommand)]
        action: ManifestAction,
    },

    /// Translate plain-English policy statements into sentinel config
    /// TOML fragments (M7.10 / sentinel #59 — AEGIS pattern).
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },

    /// Start the MCP server over stdio (Claude Code connects here)
    Mcp,

    /// Legatus — connect this sentinel as an agent-side endpoint
    /// to a consul supervisor (Consular Protocol WebSocket).
    Legatus {
        #[command(subcommand)]
        action: LegatusAction,
    },

    /// Scan marketplace and output snapshot as JSON
    Scan {
        /// Output only component counts
        #[arg(long)]
        counts_only: bool,

        /// Output only validation results
        #[arg(long)]
        validate: bool,

        /// Synchronize component counts across all marketplace text files
        #[arg(long)]
        sync_counts: bool,

        /// Generate manifest.json with SHA-256 hashes for all syncable files
        #[arg(long)]
        manifest: bool,

        /// Dry-run mode (preview changes without writing). Used with --sync-counts
        #[arg(long)]
        dry_run: bool,

        /// Override marketplace root directory (default: ~/.claude/)
        #[arg(long)]
        dir: Option<String>,
    },

    /// Show hook execution statistics
    Stats {
        #[command(subcommand)]
        action: Option<StatsAction>,
    },

    /// Aggregate session JSONL token usage by Linear ticket (SEN-7)
    Tokens {
        #[command(subcommand)]
        action: TokensAction,
    },

    /// PR review thoroughness + Codex/CodeRabbit metrics (SEN-18)
    PrReview {
        #[command(subcommand)]
        action: PrReviewAction,
    },

    /// Join SEN-7 token data with Linear estimates → tokens & $ per
    /// story point, per estimate bucket, with drift detection (SEN-13).
    CostPerPoint {
        #[command(subcommand)]
        action: CostPerPointAction,
    },

    /// Scan prompt-cache hit rate across all sessions (SEN-14)
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },

    /// Deploy frequency tracker (DORA core metric #2, SEN-9). Aggregates
    /// `~/.claude/sentinel/metrics/deploys.jsonl` into 7d/30d rolling
    /// per-repo per-env counts with DORA tier classification.
    DeployFreq {
        #[command(subcommand)]
        action: DeployFreqAction,
    },

    /// SLA breach detection engine (SEN-12). Applies rules from
    /// `~/.claude/sentinel/config/slas.toml` against a subjects JSONL,
    /// records breaches, and aggregates rolling counts.
    Sla {
        #[command(subcommand)]
        action: SlaAction,
    },

    /// ROI vs human-team baseline — joins SEN-7 + SEN-13 to compute
    /// $ Claude spent vs $ a human team would spend (SEN-15).
    Roi {
        #[command(subcommand)]
        action: RoiAction,
    },

    /// Manage browser test (used by browserbase-tester skill) state
    BrowserTest {
        #[command(subcommand)]
        action: BrowserTestAction,
    },

    /// Stage a new sentinel-engine binary with integrity verification
    Stage {
        /// Path to the new binary (default: target/release/sentinel-engine)
        #[arg(long)]
        binary: Option<String>,
    },

    /// Prune orphan state directories (e.g. persistent-tasks buckets whose
    /// originating cwd no longer exists on disk)
    Cleanup {
        #[command(subcommand)]
        action: CleanupAction,
    },

    /// Rotate the HMAC signing key (versioned, preserves old keys for verification)
    RotateKey,

    /// Re-sign all state and proof files with the current key version
    Resign,

    /// Generate standard project files (README, CLAUDE.md, LICENSE, etc.)
    Init {
        /// Preview only — show what would be created without writing
        #[arg(long)]
        dry_run: bool,

        /// Overwrite existing files
        #[arg(long)]
        force: bool,

        /// Batch mode — run across all repos under ~/Documents/GitHub/
        #[arg(long)]
        all: bool,

        /// Override target directory (default: current directory)
        #[arg(long)]
        dir: Option<String>,
    },

    /// Manage user configuration (~/.claude/sentinel/user.toml)
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Manage repo-local sentinel state (`.sentinel/` inside the repo)
    Project {
        #[command(subcommand)]
        action: ProjectAction,
    },

    /// Regenerate `~/.claude/CLAUDE.md` from the compiled template
    RegenerateClaudeMd,

    /// Find-and-replace in the CLAUDE.md template source, then regenerate
    EditClaudeMdTemplate {
        /// Unique substring to replace
        #[arg(long)]
        find: String,
        /// Replacement text
        #[arg(long)]
        replace: String,
    },

    /// Touch every mcp-router-wrapped MCP binary to trigger mass restart
    RestartAllMcps,

    /// Glass break — emergency workflow override (interactive terminal only)
    Break {
        /// Reason for the break (required for initiation)
        #[arg(long)]
        reason: Option<String>,

        /// Duration in minutes (default: 5, max: 30)
        #[arg(long)]
        duration: Option<u32>,

        /// Specific workflow to break (default: all)
        #[arg(long)]
        workflow: Option<String>,

        /// Show active break status
        #[arg(long)]
        status: bool,

        /// Cancel active break (re-engage enforcement)
        #[arg(long)]
        cancel: bool,

        /// Show break history (last 30 days)
        #[arg(long)]
        history: bool,

        /// List break state across all known sessions (use with --json for stdout JSON)
        #[arg(long)]
        list: bool,

        /// Target a specific session ID (for --status / --cancel)
        #[arg(long)]
        session: Option<String>,

        /// Output as JSON on stdout (applies to --status, --list, --history)
        #[arg(long)]
        json: bool,
    },

    /// External-benchmark eval corpus management (A12).
    ///
    /// Phase 2 ships the corpus loader + `list` subcommand. Future
    /// phases add the benchmark runner that loads cases, dispatches
    /// to agents via A2's capability router, scores against the
    /// rubric, and emits results. See `docs/a12-external-benchmarks.md`.
    Eval {
        #[command(subcommand)]
        action: EvalAction,
    },

    /// BA-orchestrator surface. Produces the verifiable
    /// recommendation envelope (citations + `requirement_refs` +
    /// `spec_challenge`) that sentinel's BA1 / BA3 / A13 gates verify
    /// downstream. Phase 3 ships the `draft` subcommand.
    Ba {
        #[command(subcommand)]
        action: BaAction,
    },

    /// Run a command and emit token-compressed output (sentinel's native
    /// "RTK"). Runs `<cmd>` to completion, structurally compresses its
    /// stdout/stderr (collapsing build/test/grep noise while preserving every
    /// error/result/warning line verbatim), prints the compressed output, and
    /// exits with the wrapped command's exit code. `SENTINEL_COMPRESS_BYPASS=1`
    /// passes output through unchanged.
    ///
    /// Usage: `sentinel compress -- cargo test --workspace`
    Compress {
        /// The command and its args (everything after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
}

/// `sentinel eval` subcommands.
#[derive(Subcommand)]
enum EvalAction {
    /// List every case registered in the BA-Eval corpus
    /// (`~/.claude/sentinel/eval/ba-corpus/`). Includes both the
    /// public test split and the private test split per spec §3.4,
    /// with the `is_private_test` flag clearly marked.
    List {
        /// Output as JSON instead of human-readable table.
        #[arg(long)]
        json: bool,

        /// Override the corpus base directory
        /// (default: `~/.claude/sentinel/eval/ba-corpus/`).
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,
    },

    /// Execute a benchmark run: load cases, replay recorded
    /// candidate outputs through the configured `EvalScorerPort`
    /// (LLM-as-judge by default), persist the run record under
    /// `~/.claude/sentinel/eval/ba-corpus/runs/{run_id}.json`.
    ///
    /// Phase 3e is replay-only — supply candidate outputs in a JSON
    /// file mapping `case_id -> output_text`. Live-LLM dispatch
    /// through the A2 router is a future phase.
    Run {
        /// Name for this run. Persisted at
        /// `{runs_dir}/{run_id}.json`. Same id on a re-run overwrites.
        #[arg(long, value_name = "ID")]
        run_id: String,

        /// Path to a JSON file mapping `case_id -> candidate_output`.
        #[arg(long, value_name = "PATH")]
        candidates: String,

        /// Restrict the run to specific `case_id`s. Repeatable.
        /// When unset, every case in the corpus runs.
        #[arg(long, value_name = "ID")]
        case_id: Vec<String>,

        /// Override the corpus base directory
        /// (default: `~/.claude/sentinel/eval/ba-corpus/`).
        #[arg(long, value_name = "PATH")]
        corpus_dir: Option<String>,

        /// Override the run-store directory
        /// (default: `~/.claude/sentinel/eval/ba-corpus/runs/`).
        #[arg(long, value_name = "PATH")]
        runs_dir: Option<String>,

        /// Emit the full `EvalRunResult` as JSON instead of a
        /// human-readable summary.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel ba` subcommands.
#[derive(Subcommand)]
enum BaAction {
    /// Draft a BA recommendation. Calls the orchestrator's
    /// `draft()` use case via the Anthropic LLM adapter
    /// (`ANTHROPIC_API_KEY` required), returns a structured
    /// [`BaRecommendation`] containing the recommendation body,
    /// citations, `requirement_refs`, and a complete A13 spec
    /// challenge. The resulting envelope is exactly what sentinel's
    /// BA1 / BA3 / A13 gates verify when it's serialized into a
    /// downstream tool's `extra` payload.
    Draft {
        /// Stakeholder brief (required). Quote to preserve
        /// whitespace.
        #[arg(long, value_name = "TEXT")]
        brief: String,

        /// Audience for the recommendation. One of:
        /// `exec`, `board`, `customer`, `internal_team`.
        #[arg(long, value_name = "AUDIENCE")]
        audience: String,

        /// Operator constraint the orchestrator must honor or
        /// surface as `constraints_not_satisfied`. Repeatable.
        #[arg(long = "constraint", value_name = "TEXT")]
        constraints: Vec<String>,

        /// Free-form identifier for the calling agent. Used for
        /// audit attribution on the emitted [`BaRecommendation`].
        #[arg(long, value_name = "ID", default_value = "ba-orchestrator")]
        agent_id: String,

        /// Emit the full [`BaRecommendation`] as pretty JSON
        /// instead of a human-readable summary.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel federation` subcommands (Apollo-style federation tooling, M2.4 + M2.8).
#[derive(Subcommand)]
enum FederationAction {
    /// Compose the federated step supergraph from `~/.claude/sentinel/config/steps/*.toml`
    /// and report any inconsistencies. Exit code 1 on errors.
    Compose {
        /// Emit machine-readable JSON instead of human-readable text.
        /// The `sentinel federation check` CI command consumes this.
        #[arg(long)]
        json: bool,

        /// Override the config directory (default: `~/.claude/sentinel/config/`).
        #[arg(long)]
        config_dir: Option<String>,
    },

    /// CI-flavored federation check for PRs (M2.8). Always emits JSON
    /// shaped for GitHub Checks API. `conclusion` is one of
    /// success/neutral/failure. Exit 1 on failure, 0 otherwise.
    Check {
        /// Override the config directory (default: `~/.claude/sentinel/config/`).
        #[arg(long)]
        config_dir: Option<String>,
    },
}

/// `sentinel manifest` subcommands (M2.13, sentinel #26).
#[derive(Subcommand)]
enum ManifestAction {
    /// Write `<config_dir>/steps/manifest.toml` covering every step
    /// TOML in the directory. With `--key-env` signs each entry with
    /// an Ed25519 key whose 32-byte hex seed lives in the named env
    /// var; without it, writes a hash-only manifest (bit-rot protection
    /// without cryptographic authenticity).
    Write {
        /// Override the config directory (default: `~/.claude/sentinel/config/`).
        #[arg(long)]
        config_dir: Option<String>,
        /// Name of an env var holding a 32-byte (64 hex char) Ed25519
        /// seed. When set, signs every entry.
        #[arg(long)]
        key_env: Option<String>,
        /// Preview only — print what would be written without touching
        /// the file.
        #[arg(long)]
        dry_run: bool,
    },

    /// Verify `<config_dir>/steps/manifest.toml` against the current
    /// step TOML files. Re-canonicalizes each source, recomputes hashes,
    /// and (in strict mode) verifies Ed25519 signatures. Exit 1 on
    /// failure.
    Verify {
        /// Override the config directory (default: `~/.claude/sentinel/config/`).
        #[arg(long)]
        config_dir: Option<String>,
        /// Hex-encoded 32-byte Ed25519 public key. Overrides any
        /// `public_key` header inside the manifest itself.
        #[arg(long)]
        pubkey: Option<String>,
        /// Force strict mode (require all signatures to verify) even
        /// when the manifest looks hash-only. By default, strict is
        /// auto-enabled when the manifest contains any signed entry
        /// AND a public key is resolvable.
        #[arg(long)]
        strict: bool,
        /// Force hash-only mode (ignore signatures). Mutually exclusive
        /// with `--strict`; takes precedence if both are passed (errors
        /// loudly).
        #[arg(long)]
        hash_only: bool,
    },

    /// Pretty-print a summary of `<config_dir>/steps/manifest.toml`:
    /// entry count, signed-vs-unsigned breakdown, public key hint,
    /// per-entry hash prefix.
    Show {
        /// Override the config directory (default: `~/.claude/sentinel/config/`).
        #[arg(long)]
        config_dir: Option<String>,
    },
}

/// `sentinel policy` subcommands (M7.10, sentinel #59).
#[derive(Subcommand)]
enum PolicyAction {
    /// Parse a one-line policy statement and emit a ready-to-paste
    /// TOML fragment for a `step_verifiers` entry.
    ///
    /// Grammar:
    ///   `<skill>/<phase>/<step> requires <adapter> [verified|provenance]`
    ///
    /// Examples:
    ///   `linear/qa-handoff/3.5.5 requires browserbase`
    ///   `linear/qa-handoff/3.5.5 requires browserbase provenance`
    Suggest {
        /// The policy statement to translate. Quote it if it contains
        /// spaces (which it almost certainly does).
        policy: String,
    },
}

/// `sentinel legatus` subcommands.
#[derive(Subcommand)]
enum LegatusAction {
    /// Connect to a consul supervisor and run a legatus session
    /// loop. Opens the Consular Protocol WebSocket, registers
    /// this sentinel as a session, sends heartbeats, logs any
    /// inbound `RelayInstructions`, and emits `SessionCompleted` on
    /// Ctrl-C. No Claude Code injection yet — that's the next
    /// commit in this series.
    Connect {
        /// Consulate WebSocket URL.
        #[arg(long, default_value = "ws://127.0.0.1:9000")]
        consulate_url: String,
        /// 32-byte bootstrap secret as 64 hex chars.
        #[arg(long, value_name = "HEX64", env = "CONSULATE_BOOTSTRAP_SECRET", hide_env_values = true)]
        bootstrap_secret: String,
        /// Operator-chosen session name hint.
        #[arg(long, default_value = "sentinel")]
        suggested_name: String,
        /// Working directory absolute path (default: current).
        #[arg(long)]
        working_dir: Option<String>,
        /// Optional branch name for collision-suffix
        /// disambiguation.
        #[arg(long)]
        branch: Option<String>,
        /// Optional initial task description.
        #[arg(long)]
        task_description: Option<String>,
        /// Heartbeat interval in seconds.
        #[arg(long, default_value_t = 20)]
        heartbeat_secs: u64,
    },
    /// Generate a 32-byte bootstrap secret (64 hex chars), suitable
    /// for use as `--legatus-bootstrap-secret` / consulate's
    /// `--bootstrap-secret`. Prints to stdout by default; with
    /// `--output <path>` writes the secret to the file (mode 0600)
    /// instead. Uses the OS CSPRNG (`OsRng`).
    Init {
        /// Optional path to write the secret to. The file is
        /// created with mode `0600`. Parent directories are
        /// created if they don't already exist. When omitted, the
        /// secret is printed to stdout and no file is written.
        #[arg(long, value_name = "PATH")]
        output: Option<String>,
        /// Overwrite the output file if it already exists. Default
        /// is to refuse — secrets shouldn't be silently rotated.
        #[arg(long)]
        force: bool,
    },
    /// Query the running daemon's `/legatus/health` endpoint and
    /// pretty-print the current connection state (and pending
    /// outbox depth). Reads the daemon port + bearer token from
    /// `~/.claude/sentinel/daemon-token`.
    Status {
        /// Emit the raw JSON instead of the pretty-printed
        /// summary. Useful for piping into `jq` / dashboards.
        #[arg(long)]
        json: bool,
    },
}

/// `sentinel project` subcommands.
#[derive(Subcommand)]
enum ProjectAction {
    /// Scaffold `.sentinel/` (repo-local sentinel state) in the current
    /// directory or a specified path. Idempotent — existing files are
    /// preserved unless `--force` is passed.
    Init {
        /// Override target directory (default: current directory).
        #[arg(long)]
        dir: Option<String>,
        /// Overwrite existing files instead of preserving them.
        #[arg(long)]
        force: bool,
        /// Preview only — show what would be created without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Append a handover stub to `.sentinel/handovers/YYYY-MM-DD-<slug>.md`.
    /// Requires the repo to have been initialized via `sentinel project init`.
    Handover {
        /// Title — becomes the document heading and filename slug.
        #[arg(long)]
        title: String,
        /// Optional pre-filled summary for the Context section.
        #[arg(long)]
        summary: Option<String>,
        /// Override target directory (default: current directory).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Append a lesson to `.sentinel/lessons/L-<NNN>.json` with the next
    /// monotonic ID. Requires the repo to have been initialized via
    /// `sentinel project init`.
    Lesson {
        /// Title — short, becomes the lesson's `title` field.
        #[arg(long)]
        title: String,
        /// Optional summary populating the `summary` field.
        #[arg(long)]
        summary: Option<String>,
        /// Tags — pass `--tag X --tag Y` for multiple. Populates `tags`.
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Override target directory (default: current directory).
        #[arg(long)]
        dir: Option<String>,
    },
}

/// `sentinel cleanup` subcommands.
#[derive(Subcommand)]
enum CleanupAction {
    /// Prune orphan `project_hash` buckets under
    /// `~/.claude/sentinel/persistent-tasks/` whose cwd no longer exists.
    /// Default mode is dry-run; pass `--apply` to actually remove them.
    PersistentTasks {
        /// Actually remove orphan buckets. Without this flag, only the
        /// audit report is printed.
        #[arg(long)]
        apply: bool,
    },
    /// Prune orphan session task directories under `~/.claude/tasks/`
    /// whose `session_id` (the directory name) does not appear as a
    /// `.jsonl` transcript file under any project in
    /// `~/.claude/projects/`. Older-than gating defends against active
    /// sessions whose transcript hasn't been written yet.
    Tasks {
        /// Minimum age in days for a directory to be considered for
        /// cleanup. Defaults to 30 — only directories older than this
        /// are candidates, regardless of whether their `session_id` is
        /// orphan. Younger orphans are kept (the transcript may be
        /// about to write).
        #[arg(long, default_value = "30")]
        older_than: u64,
        /// Actually remove orphan directories. Without this flag, only
        /// the audit report is printed.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Set a config value (e.g., `sentinel config set name "Gary"`)
    Set {
        /// Config key (currently: name)
        key: String,
        /// Value to set
        value: String,
    },
    /// Show current configuration
    Show,
}

#[derive(Subcommand)]
enum StatsAction {
    /// Show hook invocation summary from hook-invocations.jsonl
    /// (top-N by call count, latency, and block frequency).
    Hooks {
        /// Limit the number of rows shown per breakdown (default 10).
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Only consider invocations from the last N hours (default 24).
        #[arg(long, default_value_t = 24)]
        hours: u32,
    },
}

#[derive(Subcommand)]
enum TokensAction {
    /// Walk ~/.claude/projects/, aggregate per-ticket token cost,
    /// write ~/.claude/sentinel/metrics/tokens-per-ticket.jsonl.
    Scan {
        /// Number of top-cost tickets to print (default 10)
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
}

#[derive(Subcommand)]
enum PrReviewAction {
    /// Walk merged PRs across firefly-pro + sentinel via `gh`,
    /// write pr-review.jsonl + pr-review-summary.json.
    Scan {
        /// Window in days to scan (default 30)
        #[arg(long, default_value_t = 30)]
        days: u32,
    },
}

#[derive(Subcommand)]
enum CostPerPointAction {
    /// Join tokens-per-ticket.jsonl with Linear estimates, write
    /// ~/.claude/sentinel/metrics/cost-per-point.{jsonl,-summary.json}.
    Scan,
}

#[derive(Subcommand)]
enum CacheAction {
    /// Walk ~/.claude/projects/, compute per-session cache hit rate,
    /// write ~/.claude/sentinel/metrics/cache-efficiency.{jsonl,-summary.json}.
    Scan {
        /// Number of worst sessions to print (default 10)
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
}

#[derive(Subcommand)]
enum RoiAction {
    /// Join SEN-7 tokens-per-ticket data with SEN-13 cost-per-point
    /// summary, project ROI vs $327/point human baseline, write
    /// ~/.claude/sentinel/metrics/roi.{jsonl,-summary.json}.
    Scan,
}

#[derive(Subcommand)]
enum SlaAction {
    /// Apply rules against a subjects JSONL; record breaches.
    Check {
        /// Path to slas.toml (default: ~/.claude/sentinel/config/slas.toml)
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// JSONL file of Subject records to check.
        #[arg(long)]
        subjects: std::path::PathBuf,
        /// Print matches but don't append to sla-breaches.jsonl.
        #[arg(long)]
        dry_run: bool,
    },
    /// Roll up sla-breaches.jsonl into 24h/7d/30d counts per SLA.
    Aggregate,
    /// Print a starter slas.toml to stdout.
    Template,
}

#[derive(Subcommand)]
enum DeployFreqAction {
    /// Read deploys.jsonl, write deploys-summary.json, print summary.
    Aggregate,
    /// Manually append a deploy record (testing + backfill path before
    /// Hookdeck `deployment.success` ingest lands).
    Record {
        /// Repository identifier (e.g. firefly-pro-crm).
        #[arg(long)]
        repo: String,
        /// Environment (prod / staging / preview).
        #[arg(long)]
        env: String,
        /// Git commit SHA the deploy shipped.
        #[arg(long)]
        commit: String,
        /// Pipeline duration in seconds (optional).
        #[arg(long)]
        duration_s: Option<u64>,
        /// RFC3339 timestamp; defaults to `now()` when omitted.
        #[arg(long)]
        timestamp: Option<String>,
    },
}

#[derive(Subcommand)]
enum BrowserTestAction {
    /// Record a passing browser test for the current session
    Record {
        /// Session ID (reads from stdin if not provided)
        #[arg(long)]
        session: Option<String>,
    },
    /// Check if a valid browser test exists for the current session
    Check {
        /// Session ID
        #[arg(long)]
        session: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon {
            port,
            legatus_consulate_url,
            legatus_consulate_failover_urls,
            legatus_bootstrap_secret,
            legatus_suggested_name,
            legatus_working_dir,
            legatus_heartbeat_secs,
            legatus_witness_verify,
            legatus_praefectus_url,
            legatus_praefectus_token,
            legatus_operator_id,
        } => {
            let witness_verify = match legatus_witness_verify.as_str() {
                "in-memory" => daemon_cmd::WitnessVerifyMode::InMemory,
                "http" => {
                    let base_url = legatus_praefectus_url.ok_or_else(|| {
                        anyhow::anyhow!(
                            "--legatus-witness-verify=http requires \
                             --legatus-praefectus-url <URL>"
                        )
                    })?;
                    let bearer_token = legatus_praefectus_token.ok_or_else(|| {
                        anyhow::anyhow!(
                            "--legatus-witness-verify=http requires \
                             --legatus-praefectus-token <TOKEN> \
                             (or LEGATUS_PRAEFECTUS_TOKEN env)"
                        )
                    })?;
                    daemon_cmd::WitnessVerifyMode::Http {
                        base_url,
                        bearer_token,
                    }
                }
                _ => daemon_cmd::WitnessVerifyMode::None,
            };
            daemon_cmd::run(
                port,
                daemon_cmd::LegatusOptions {
                    consulate_url: legatus_consulate_url,
                    failover_urls: legatus_consulate_failover_urls,
                    bootstrap_secret_hex: legatus_bootstrap_secret,
                    suggested_name: legatus_suggested_name,
                    working_dir: legatus_working_dir,
                    heartbeat_secs: legatus_heartbeat_secs,
                    witness_verify,
                    operator_id: legatus_operator_id,
                },
            )
            .await
        },
        Commands::Stop { wait_secs } => daemon_cmd::run_stop(wait_secs),
        Commands::Hook {
            event,
            matcher,
            standalone,
        } => hook_cmd::run(&event, matcher.as_deref(), standalone).await,
        Commands::HookInternal {
            event,
            matcher,
            standalone,
        } => hook_cmd::run_internal(&event, matcher.as_deref(), standalone).await,
        Commands::Verify { session } => verify_cmd::run(&session),
        Commands::Federation { action } => match action {
            FederationAction::Compose { json, config_dir } => federation_cmd::run(json, config_dir),
            FederationAction::Check { config_dir } => federation_cmd::run_check(config_dir),
        },
        Commands::Manifest { action } => run_manifest(action),
        Commands::Policy { action } => run_policy(action),
        Commands::Mcp => mcp_cmd::run().await,
        Commands::Eval { action } => match action {
            EvalAction::List { json, dir } => eval_cmd::list(json, dir),
            EvalAction::Run {
                run_id,
                candidates,
                case_id,
                corpus_dir,
                runs_dir,
                json,
            } => eval_cmd::run(eval_cmd::RunArgs {
                run_id,
                candidates_path: candidates,
                case_ids: case_id,
                corpus_dir,
                runs_dir,
                json,
            }),
        },
        Commands::Ba { action } => match action {
            BaAction::Draft {
                brief,
                audience,
                constraints,
                agent_id,
                json,
            } => {
                ba_cmd::draft(ba_cmd::DraftArgs {
                    brief,
                    audience,
                    constraints,
                    agent_id,
                    json,
                })
                .await
            }
        },
        Commands::Compress { cmd } => {
            // Propagate the wrapped command's exit code so callers (and the
            // PreToolUse rewrite) see the real success/failure.
            let code = compress_cmd::run(&cmd)?;
            std::process::exit(code);
        }
        Commands::Legatus { action } => match action {
            LegatusAction::Connect {
                consulate_url,
                bootstrap_secret,
                suggested_name,
                working_dir,
                branch,
                task_description,
                heartbeat_secs,
            } => {
                legatus_cmd::run_connect(
                    &consulate_url,
                    &bootstrap_secret,
                    &suggested_name,
                    working_dir.as_deref(),
                    branch,
                    task_description,
                    heartbeat_secs,
                )
                .await
            },
            LegatusAction::Init { output, force } => legatus_cmd::run_init(output, force),
            LegatusAction::Status { json } => legatus_cmd::run_status(json).await,
        },
        Commands::Scan {
            counts_only,
            validate,
            sync_counts,
            manifest,
            dry_run,
            dir,
        } => scan_cmd::run(counts_only, validate, sync_counts, manifest, dry_run, dir),
        Commands::Stats { action } => match action {
            None => stats_cmd::run(),
            Some(StatsAction::Hooks { limit, hours }) => stats_cmd::run_hooks(limit, hours),
        },
        Commands::Tokens { action } => match action {
            TokensAction::Scan { top } => tokens_cmd::run(top),
        },
        Commands::PrReview { action } => match action {
            PrReviewAction::Scan { days } => pr_review_cmd::run(days),
        },
        Commands::CostPerPoint { action } => match action {
            CostPerPointAction::Scan => cost_per_point_cmd::run(),
        },
        Commands::Cache { action } => match action {
            CacheAction::Scan { top } => cache_cmd::run(top),
        },
        Commands::Roi { action } => match action {
            RoiAction::Scan => roi_cmd::run(),
        },
        Commands::DeployFreq { action } => match action {
            DeployFreqAction::Aggregate => deploy_freq_cmd::run_aggregate(),
            DeployFreqAction::Record {
                repo,
                env,
                commit,
                duration_s,
                timestamp,
            } => deploy_freq_cmd::run_record(repo, env, commit, duration_s, timestamp),
        },
        Commands::Sla { action } => match action {
            SlaAction::Check {
                config,
                subjects,
                dry_run,
            } => {
                let cfg = config.unwrap_or_else(|| {
                    dirs::home_dir().map_or_else(|| std::path::PathBuf::from("slas.toml"), |h| {
                            h.join(".claude")
                                .join("sentinel")
                                .join("config")
                                .join("slas.toml")
                        })
                });
                sla_cmd::run_check(cfg, subjects, dry_run)
            }
            SlaAction::Aggregate => sla_cmd::run_aggregate(),
            SlaAction::Template => {
                sla_cmd::run_template();
                Ok(())
            }
        },
        Commands::BrowserTest { action } => match action {
            BrowserTestAction::Record { session } => browser_test_cmd::record(session),
            BrowserTestAction::Check { session } => browser_test_cmd::check(session),
        },
        Commands::Stage { binary } => stage_cmd::run(binary),
        Commands::Cleanup { action } => match action {
            CleanupAction::PersistentTasks { apply } => cleanup_cmd::run_persistent_tasks(apply),
            CleanupAction::Tasks { older_than, apply } => {
                cleanup_cmd::run_session_tasks(older_than, apply)
            }
        },
        Commands::RotateKey => rotate_key_cmd::run(),
        Commands::Resign => resign_cmd::run(),
        Commands::Init {
            dry_run,
            force,
            all,
            dir,
        } => init_cmd::run(dry_run, force, all, dir),
        Commands::Config { action } => match action {
            ConfigAction::Set { key, value } => config_cmd::set(&key, &value),
            ConfigAction::Show => config_cmd::show(),
        },
        Commands::Project { action } => match action {
            ProjectAction::Init {
                dir,
                force,
                dry_run,
            } => project_cmd::run(dir.map(std::path::PathBuf::from), force, dry_run),
            ProjectAction::Handover { title, summary, dir } => {
                project_cmd::run_handover(
                    dir.map(std::path::PathBuf::from),
                    title,
                    summary,
                )
            }
            ProjectAction::Lesson {
                title,
                summary,
                tags,
                dir,
            } => project_cmd::run_lesson(
                dir.map(std::path::PathBuf::from),
                title,
                summary,
                tags,
            ),
        },
        Commands::RegenerateClaudeMd => {
            let result = claude_md_cmd::regenerate()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Commands::EditClaudeMdTemplate { find, replace } => {
            let result = claude_md_cmd::edit_template(&find, &replace)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Commands::RestartAllMcps => {
            let result = claude_md_cmd::restart_all_mcps()?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Commands::Break {
            reason,
            duration,
            workflow,
            status,
            cancel,
            history,
            list,
            session,
            json,
        } => {
            break_cmd::run(
                reason, duration, workflow, status, cancel, history, list, session, json,
            )
            .await
        }
    }
}

/// Resolve a config dir from a CLI flag or the sentinel default.
fn resolve_config_dir(override_str: Option<String>) -> std::path::PathBuf {
    match override_str {
        Some(s) => std::path::PathBuf::from(s),
        None => sentinel_infrastructure::config::config_dir(),
    }
}

/// Dispatch `sentinel manifest` subcommands. Returns a process exit code:
/// 0 on success, 1 on any failure or verify mismatch.
fn run_manifest(action: ManifestAction) -> anyhow::Result<()> {
    match action {
        ManifestAction::Write {
            config_dir,
            key_env,
            dry_run,
        } => {
            let cd = resolve_config_dir(config_dir);
            manifest_cmd::run_write(manifest_cmd::WriteOptions {
                config_dir: cd,
                key_env,
                dry_run,
            })
        }
        ManifestAction::Verify {
            config_dir,
            pubkey,
            strict,
            hash_only,
        } => {
            if strict && hash_only {
                anyhow::bail!("--strict and --hash-only are mutually exclusive");
            }
            let strict_override = if strict {
                Some(true)
            } else if hash_only {
                Some(false)
            } else {
                None
            };
            let cd = resolve_config_dir(config_dir);
            let report = manifest_cmd::run_verify(manifest_cmd::VerifyOptions {
                config_dir: cd,
                pubkey_hex: pubkey,
                strict: strict_override,
            })?;
            use sentinel_domain::step_manifest::ManifestCheck;
            for entry in &report.entries {
                let tag = match &entry.result {
                    Ok(ManifestCheck::SignedOk) => "OK (signed)".to_string(),
                    Ok(ManifestCheck::HashOnlyOk) => "OK (hash)".to_string(),
                    Err(e) => format!("FAIL: {e}"),
                };
                println!("  {} {tag}", entry.name);
            }
            println!(
                "summary: {} signed-ok, {} hash-ok, {} failures",
                report.signed_ok,
                report.hash_only_ok,
                report.failures.len()
            );
            if !report.ok() {
                anyhow::bail!("manifest verify failed for {} entries", report.failures.len());
            }
            Ok(())
        }
        ManifestAction::Show { config_dir } => {
            let cd = resolve_config_dir(config_dir);
            manifest_cmd::run_show(&cd)
        }
    }
}

/// Dispatch `sentinel policy` subcommands (M7.10, sentinel #59).
fn run_policy(action: PolicyAction) -> anyhow::Result<()> {
    match action {
        PolicyAction::Suggest { policy } => policy_cmd::run_suggest(&policy),
    }
}
