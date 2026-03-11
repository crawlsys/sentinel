//! Dashboard API Router
//!
//! Full REST API for the Sentinel dashboard.
//! Provides endpoints for proof chains, workflows, and hook stats.

pub mod hooks;
pub mod logs;
pub mod proofs;
pub mod scan;
pub mod sessions;
pub mod store;
pub mod workflows;

use std::sync::Arc;

use axum::Router;
use sentinel_domain::state::SessionState;
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
        .nest("/api/sentinel", sessions::router())
        .nest("/api", scan::router())
        .nest("/api", logs::router())
        .nest("/api", store::router())
        .with_state(state)
}

async fn health() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "engine": "sentinel"
    }))
}
