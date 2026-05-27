//! sentinel-bridge — Rust port of tools/sentinel-viz/sentinel_bridge.py
//! + the harness-shims/*.py scripts.
//!
//! Two responsibilities:
//!   1. Run per-harness shims that translate non-Claude session output
//!      (codex only — opencode/qwen/gemini shims still live under
//!      shims/ but are dormant; see the Shim enum below) into the
//!      bridge's hook-invocation JSONL format.
//!   2. Tail every metrics dir's hook-invocations.jsonl + sessions.jsonl
//!      and persist into the activegraph-compatible SQLite store that
//!      sentinel-viz-api serves.
//!
//! Subcommands:
//!   sentinel-bridge tail           — full ingest loop (default cadence)
//!   sentinel-bridge backfill       — one-shot pass, exit
//!   sentinel-bridge shim <name>    — run a single shim once
//!
//! Schema parity with the retired Python bridge is verified in
//! tests/parity.rs against fixture JSONLs.

mod ingest;
mod jsonl;
mod shims;
mod store;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::thread::sleep;
use std::time::Duration;
use tracing::{info, warn};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const SHIM_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(name = "sentinel-bridge", version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Continuous tail mode: run shims + ingest in a loop.
    Tail,
    /// One-shot pass: run every shim once, ingest, exit.
    Backfill,
    /// Run a single harness shim once and exit.
    Shim {
        #[arg(value_enum)]
        name: Shim,
    },
}

// Allowlist: only claude + codex are tracked. opencode/qwen/gemini
// shims still live under shims/ for the option of revival, but the
// CLI no longer accepts them as values and `Shim::All` only fires
// codex. The viz frontend's HARNESSES list mirrors this.
#[derive(Clone, Debug, clap::ValueEnum)]
enum Shim {
    Codex,
    All,
}

fn run_shim(name: &Shim) -> Result<usize> {
    match name {
        Shim::Codex | Shim::All => shims::codex::run_once().or_else(|e| {
            warn!("codex shim failed: {e}");
            Ok(0)
        }),
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Shim { name } => {
            let n = run_shim(&name)?;
            info!("emitted {n} records");
        }
        Cmd::Backfill => {
            backfill()?;
        }
        Cmd::Tail => {
            tail_loop()?;
        }
    }
    Ok(())
}

fn backfill() -> Result<()> {
    let n_shim = run_shim(&Shim::All)?;
    info!("shims emitted {n_shim} new records");

    let store_path = ingest::store_path();
    let mut store = store::Store::open(&store_path)?;
    let mut state = jsonl::OffsetState::load(&ingest::offset_state_path())?;
    let n = ingest::run_pass(&mut store, &mut state)?;
    state.save(&ingest::offset_state_path())?;
    info!(
        "ingested {n} hook records; store={} run={}",
        store_path.display(),
        store.run_id()
    );
    Ok(())
}

fn tail_loop() -> Result<()> {
    let store_path = ingest::store_path();
    info!("opening store {}", store_path.display());
    let mut store = store::Store::open(&store_path)?;
    let mut state = jsonl::OffsetState::load(&ingest::offset_state_path())?;
    info!("run_id={}", store.run_id());

    let mut next_shim_tick = std::time::Instant::now();
    loop {
        // Shim pass occasionally — they're heavier than the metric
        // tail, so they don't need to fire on every poll.
        if std::time::Instant::now() >= next_shim_tick {
            let n_shim = run_shim(&Shim::All).unwrap_or_else(|e| {
                warn!("shims failed: {e}");
                0
            });
            if n_shim > 0 {
                info!("shims +{n_shim}");
            }
            next_shim_tick = std::time::Instant::now() + SHIM_INTERVAL;
        }

        match ingest::run_pass(&mut store, &mut state) {
            Ok(n) if n > 0 => {
                info!("ingested +{n}");
                state.save(&ingest::offset_state_path()).ok();
            }
            Ok(_) => {}
            Err(e) => warn!("ingest failed: {e}"),
        }

        sleep(POLL_INTERVAL);
    }
}
