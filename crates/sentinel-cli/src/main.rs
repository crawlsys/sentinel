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
mod daemon_cmd;
mod hook_cmd;
mod mcp_cmd;
mod scan_cmd;
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
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon { port } => daemon_cmd::run(port).await,
        Commands::Hook {
            event,
            matcher,
            standalone,
        } => hook_cmd::run(&event, matcher.as_deref(), standalone).await,
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
    }
}
