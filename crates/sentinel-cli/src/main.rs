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
mod daemon_cmd;
mod hook_cmd;
mod init_cmd;
mod mcp_cmd;
mod resign_cmd;
mod rotate_key_cmd;
mod scan_cmd;
mod stage_cmd;
mod stats_cmd;
mod steel_test_cmd;
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
    Stats,

    /// Manage Steel browser test state
    SteelTest {
        #[command(subcommand)]
        action: SteelTestAction,
    },

    /// Stage a new sentinel-engine binary with integrity verification
    Stage {
        /// Path to the new binary (default: target/release/sentinel-engine.exe)
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

        /// Output history as JSON (use with --history)
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SteelTestAction {
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
        Commands::Verify { session } => verify_cmd::run(&session).await,
        Commands::Mcp => mcp_cmd::run().await,
        Commands::Scan {
            counts_only,
            validate,
            sync_counts,
            manifest,
            dry_run,
            dir,
        } => scan_cmd::run(counts_only, validate, sync_counts, manifest, dry_run, dir).await,
        Commands::Stats => stats_cmd::run().await,
        Commands::SteelTest { action } => match action {
            SteelTestAction::Record { session } => steel_test_cmd::record(session).await,
            SteelTestAction::Check { session } => steel_test_cmd::check(session).await,
        },
        Commands::Stage { binary } => stage_cmd::run(binary).await,
        Commands::RotateKey => rotate_key_cmd::run().await,
        Commands::Resign => resign_cmd::run().await,
        Commands::Init {
            dry_run,
            force,
            all,
            dir,
        } => init_cmd::run(dry_run, force, all, dir).await,
        Commands::Break {
            reason,
            duration,
            workflow,
            status,
            cancel,
            history,
            json,
        } => break_cmd::run(reason, duration, workflow, status, cancel, history, json).await,
    }
}
