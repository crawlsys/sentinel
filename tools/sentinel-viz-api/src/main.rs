use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use sentinel_viz_api::{db, server::AppState};

#[tokio::main]
async fn main() -> Result<()> {
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
        // Deep enough for 10-15m demo sparklines even with hook
        // fanout; operators can still override with
        // SENTINEL_VIZ_WINDOW.
        .unwrap_or(6_000);

    // Build LLM state up-front: NamingState/SummaryState now probe
    // the local backend (`OLLAMA_HOST`) before deciding whether to
    // route to local Ollama or fall through to OpenRouter. The
    // probe is async + bounded by PROBE_TIMEOUT_MS (1.5s) so total
    // startup cost is at most ~3s when both probes time out.
    let naming = sentinel_viz_api::naming::NamingState::from_env().await;
    let summary = sentinel_viz_api::summary::SummaryState::from_env().await;
    let rollup = sentinel_viz_api::rollup_summary::RollupState::from_env().await;
    let state = Arc::new(AppState {
        db_path: db_path.clone(),
        window_limit,
        started_at: Instant::now(),
        cache: std::sync::RwLock::new(Vec::new()),
        activity_cache: std::sync::RwLock::new(Vec::new()),
        naming,
        summary,
        rollup,
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
