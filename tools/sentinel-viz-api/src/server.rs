use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

use crate::activity;
use crate::db;
use crate::graph::{self, GraphOpts};
use crate::health;
use crate::model::{ActivityResponse, GraphResponse};
use crate::sse;

pub struct AppState {
    pub db_path: PathBuf,
    pub window_limit: usize,
    pub started_at: Instant,
}

pub fn router(state: Arc<AppState>) -> axum::Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    axum::Router::new()
        .route("/api/healthz", get(health::healthz))
        .route("/api/graph", get(graph_endpoint))
        .route("/api/activity/{session_id}", get(activity_endpoint))
        .route("/api/stream", get(sse::stream))
        .layer(cors)
        .with_state(state)
}

#[derive(Deserialize)]
pub struct GraphQuery {
    pub limit: Option<usize>,
    /// Drop `sentinel.*` events older than this many seconds.
    /// Default 6h. Pass `0` to disable the floor.
    pub since_secs: Option<i64>,
    /// `true` keeps `SentinelHookInvocation` nodes in the response.
    /// Default `false` collapses them and synthesises direct
    /// session → tool-call edges.
    pub include_hooks: Option<bool>,
}

async fn graph_endpoint(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GraphQuery>,
) -> Result<Json<GraphResponse>, (StatusCode, String)> {
    let limit = q.limit.unwrap_or(state.window_limit);
    let since_secs = match q.since_secs {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(6 * 3600),
    };
    let include_hooks = q.include_hooks.unwrap_or(false);
    let conn = db::open_ro(&state.db_path).map_err(internal)?;
    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit, since_secs, include_hooks },
    )
    .map_err(internal)?;
    Ok(Json(g))
}

#[derive(Deserialize)]
pub struct ActivityQuery {
    pub limit: Option<usize>,
    pub at_ts: Option<String>,
    pub window: Option<i64>,
}

async fn activity_endpoint(
    State(_state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Query(q): Query<ActivityQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(80);
    let window = q.window.unwrap_or(30);
    let a: ActivityResponse = activity::session_activity(&session_id, limit, q.at_ts.as_deref(), window);
    Json(a)
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
