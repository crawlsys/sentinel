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
        .unwrap_or(100);

    let state = Arc::new(AppState {
        db_path: db_path.clone(),
        window_limit,
        started_at: Instant::now(),
        cache: std::sync::RwLock::new(Vec::new()),
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
