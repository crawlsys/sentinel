//! `sentinel daemon` — Starts MCP server + hook listener + dashboard API

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;
use tracing::info;

use sentinel_domain::state::SessionState;

pub async fn run(port: u16) -> Result<()> {
    info!("Sentinel daemon starting on port {port}");

    let state = Arc::new(RwLock::new(SessionState::new("daemon")));

    // Start dashboard API server
    let app = axum::Router::new()
        .route("/api/health", axum::routing::get(health))
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        );

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    info!("Dashboard API listening on http://localhost:{port}");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": "sentinel"
    }))
}
