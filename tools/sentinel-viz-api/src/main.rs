use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use sentinel_viz_api::{db, server::AppState};

#[tokio::main]
async fn main() -> Result<()> {
    // `sentinel-viz-api ingest [--tail] [--store PATH]` — the metrics→events
    // ingester (Rust replacement for the former sentinel_bridge.py). Handled
    // before the server path so it stays a plain synchronous import.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("ingest") {
        return run_ingest(args.collect());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let db_path = db::default_db_path()?;
    let port: u16 = std::env::var("SENTINEL_VIZ_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8082);
    let host: String =
        std::env::var("SENTINEL_VIZ_API_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let window_limit: usize = std::env::var("SENTINEL_VIZ_WINDOW")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);

    // `/api/config` may bind an OpenAI key and Ollama URL into server
    // state, so it must not be writable when the server is reachable
    // off the loopback interface.
    let allow_config_writes = sentinel_viz_api::server::is_loopback_host(&host);
    if !allow_config_writes {
        tracing::warn!(
            %host,
            "bound to a non-loopback host; POST /api/config is disabled"
        );
    }

    let state = Arc::new(AppState {
        db_path: db_path.clone(),
        window_limit,
        started_at: Instant::now(),
        allow_config_writes,
        cache: std::sync::RwLock::new(Vec::new()),
        activity_cache: std::sync::RwLock::new(Vec::new()),
        naming: sentinel_viz_api::naming::NamingState::from_env(),
        summary: sentinel_viz_api::summary::SummaryState::from_env(),
    });

    let app = sentinel_viz_api::server::router(state);
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, db = %db_path.display(), "sentinel-viz-api listening");
    println!(
        "sentinel-viz-api · http://{addr}/  (db={})",
        db_path.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

/// Parse the `ingest` subcommand args and dispatch one-shot or `--tail`.
/// The store defaults to `db::default_db_path()` (honoring `$SENTINEL_VIZ_DB`),
/// overridable with `--store PATH`.
fn run_ingest(args: Vec<String>) -> Result<()> {
    use sentinel_viz_api::ingest::{self, MetricsPaths};

    let mut tail = false;
    let mut store: Option<std::path::PathBuf> = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--tail" => tail = true,
            "--store" => {
                store = Some(
                    it.next()
                        .map(std::path::PathBuf::from)
                        .ok_or_else(|| anyhow::anyhow!("--store requires a PATH argument"))?,
                );
            }
            other => anyhow::bail!("unknown ingest argument: {other}"),
        }
    }

    let store = match store {
        Some(p) => p,
        None => db::default_db_path()?,
    };
    let paths = MetricsPaths::from_home()?;

    if tail {
        ingest::run_tail(&store, &paths)
    } else {
        ingest::run_one_shot(&store, &paths)
    }
}
