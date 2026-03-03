//! Sentinel — Proof-of-Work Hook Engine + MCP Server
//!
//! Usage:
//!   sentinel daemon     — Start MCP server + hook listener + dashboard API
//!   sentinel hook       — Thin client, forwards to daemon (or standalone)
//!   sentinel verify     — Verify a session's proof chain
//!   sentinel mcp        — MCP server over stdio (Claude Code connects here)
//!   sentinel stats      — Hook execution statistics

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod api;
mod daemon_cmd;
mod hook_cmd;
mod mcp_cmd;
mod stats_cmd;
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

    /// Show hook execution statistics
    Stats,
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
        Commands::Stats => stats_cmd::run().await,
    }
}
