//! Local API Router
//!
//! Full REST API for local Sentinel control surfaces.
//! Provides endpoints for proof chains, workflows, and hook stats.

pub mod hooks;
pub mod logs;
pub mod memories;
pub mod memory;
mod operational_read_audit;
pub mod proofs;
pub mod scan;
pub mod sessions;
pub mod store;
pub mod workflows;

use std::sync::Arc;

use axum::{http::StatusCode, Json, Router};
use sentinel_domain::state::SessionState;
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;
use tokio::sync::RwLock;

/// Application state shared across all API handlers
#[derive(Clone)]
pub struct AppState {
    pub session: Arc<RwLock<SessionState>>,
}

/// Build the full API router
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", axum::routing::get(health))
        .nest("/api/proofs", proofs::router())
        .nest("/api/workflows", workflows::router())
        .nest("/api/hooks", hooks::router())
        .nest("/api/memories", memories::router())
        .nest("/api/memory", memory::router())
        .nest("/api/sentinel", sessions::router())
        .nest("/api", scan::router())
        .nest("/api", logs::router())
        .nest("/api", store::router())
        .with_state(state)
}

async fn health() -> Result<Json<serde_json::Value>, StatusCode> {
    let response = serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": "sentinel"
    });
    operational_read_audit::attach_operational_api_read_graph_audit(
        OperationalApiReadSurface::RootHealth,
        response,
    )
    .await
    .map(Json)
    .map_err(|error| {
        tracing::error!(
            error = %error,
            "root health API read graph audit failed; refusing unaudited response"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    })
}
