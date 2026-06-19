//! Hook Stats API Endpoints
//!
//! GET /api/hooks/stats  — execution statistics
//! GET /api/hooks/health — health status

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;

use super::{operational_read_audit, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(stats))
        .route("/health", get(health))
}

async fn stats(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    // Drop the read-guard before building the response to release it early.
    let (total_invocations, total_blocked, per_hook, per_hook_time_ms) = {
        let session = state.session.read().await;
        let hook_stats = &session.hook_stats;
        (
            hook_stats.total_invocations,
            hook_stats.total_blocked,
            hook_stats.per_hook.clone(),
            hook_stats.per_hook_time_ms.clone(),
        )
    };
    operational_json(
        OperationalApiReadSurface::HookStats,
        serde_json::json!({
            "total_invocations": total_invocations,
            "total_blocked": total_blocked,
            "per_hook": per_hook,
            "per_hook_time_ms": per_hook_time_ms,
        }),
    )
    .await
}

async fn health(State(state): State<AppState>) -> Result<Json<serde_json::Value>, StatusCode> {
    let response = {
        let session = state.session.read().await;
        serde_json::json!({
            "active": session.active,
            "session_id": session.session_id,
            "active_skill": session.active_skill,
            "uptime_since": session.started_at.to_rfc3339(),
        })
    };
    operational_json(OperationalApiReadSurface::HookHealth, response).await
}

async fn operational_json(
    surface: OperationalApiReadSurface,
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    operational_read_audit::attach_operational_api_read_graph_audit(surface, response)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(
                surface = sentinel_infrastructure::operational_api_read_graph::operational_api_read_surface_label(surface),
                error = %error,
                "operational hooks API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}
