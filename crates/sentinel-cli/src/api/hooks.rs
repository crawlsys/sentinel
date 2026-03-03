//! Hook Stats API Endpoints
//!
//! GET /api/hooks/stats  — execution statistics
//! GET /api/hooks/health — health status

use axum::{extract::State, routing::get, Json, Router};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(stats))
        .route("/health", get(health))
}

async fn stats(State(state): State<AppState>) -> Json<serde_json::Value> {
    let session = state.session.read().await;
    let stats = &session.hook_stats;
    Json(serde_json::json!({
        "total_invocations": stats.total_invocations,
        "total_blocked": stats.total_blocked,
        "per_hook": stats.per_hook,
        "per_hook_time_ms": stats.per_hook_time_ms,
    }))
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let session = state.session.read().await;
    Json(serde_json::json!({
        "active": session.active,
        "session_id": session.session_id,
        "active_skill": session.active_skill,
        "uptime_since": session.started_at.to_rfc3339(),
    }))
}
