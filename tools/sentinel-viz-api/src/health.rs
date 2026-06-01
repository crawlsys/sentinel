use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::extract::State;

use crate::db;
use crate::model::HealthResponse;
use crate::server::AppState;

pub async fn healthz(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let db_max_seq = db::open_ro(&state.db_path)
        .ok()
        .and_then(|c| db::peek_max_seq(&c).ok())
        .unwrap_or(-1);
    let uptime_sec = Instant::now().duration_since(state.started_at).as_secs();
    Json(HealthResponse {
        ok: true,
        db_max_seq,
        uptime_sec,
    })
}
