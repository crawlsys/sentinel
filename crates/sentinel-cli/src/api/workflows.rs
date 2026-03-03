//! Workflow Status API Endpoints
//!
//! GET /api/workflows              — list all workflow definitions
//! GET /api/workflows/:skill/status — current workflow state for a skill

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_workflows))
        .route("/{skill}/status", get(get_status))
}

async fn list_workflows(State(state): State<AppState>) -> Json<serde_json::Value> {
    let session = state.session.read().await;
    let workflows: Vec<serde_json::Value> = session
        .workflows
        .iter()
        .map(|(skill, wf)| {
            serde_json::json!({
                "skill": skill,
                "current_phase": wf.current_phase,
                "completed_phases": wf.completed_phases,
                "complete": wf.complete,
            })
        })
        .collect();
    Json(serde_json::json!({ "workflows": workflows }))
}

async fn get_status(
    State(state): State<AppState>,
    Path(skill): Path<String>,
) -> Json<serde_json::Value> {
    let session = state.session.read().await;
    match session.workflows.get(&skill) {
        Some(wf) => Json(serde_json::to_value(wf).unwrap_or_default()),
        None => Json(serde_json::json!({ "error": format!("No workflow for skill '{}'", skill) })),
    }
}
