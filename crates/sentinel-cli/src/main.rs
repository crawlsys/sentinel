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
mod config_cmd;
mod cost_per_point_cmd;
mod daemon_cmd;
mod hook_cmd;
mod init_cmd;
mod mcp_cmd;
mod pr_review_cmd;
mod resign_cmd;
mod roi_cmd;
mod rotate_key_cmd;
mod scan_cmd;
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

    /// Start the MCP server over stdio (Claude Code connects here)
    Mcp,

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
        Commands::Daemon { port } => daemon_cmd::run(port).await,
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
        Commands::Mcp => mcp_cmd::run().await,
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
        Commands::BrowserTest { action } => match action {
            BrowserTestAction::Record { session } => browser_test_cmd::record(session),
            BrowserTestAction::Check { session } => browser_test_cmd::check(session),
        },
        Commands::Stage { binary } => stage_cmd::run(binary),
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
