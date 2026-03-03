//! Proof Chain API Endpoints
//!
//! GET /api/proofs                    — list all proof chain sessions
//! GET /api/proofs/:session_id        — full proof chain for a session
//! GET /api/proofs/:session_id/verify — re-verify chain integrity

use axum::{
    extract::{Path, State},
    routing::get,
    Json, Router,
};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_proofs))
        .route("/{session_id}", get(get_proof_chain))
        .route("/{session_id}/verify", get(verify_chain))
}

async fn list_proofs(State(state): State<AppState>) -> Json<serde_json::Value> {
    let session = state.session.read().await;
    let chains: Vec<serde_json::Value> = session
        .proof_chains
        .iter()
        .map(|(skill, chain)| {
            serde_json::json!({
                "skill": skill,
                "session_id": chain.session_id,
                "phases": chain.proofs.len(),
                "complete": chain.complete,
                "chain_valid": chain.chain_valid,
            })
        })
        .collect();
    Json(serde_json::json!({ "chains": chains }))
}

async fn get_proof_chain(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<serde_json::Value> {
    // Try in-memory first
    let session = state.session.read().await;
    for (_skill, chain) in &session.proof_chains {
        if chain.session_id == session_id {
            return Json(serde_json::to_value(chain).unwrap_or_default());
        }
    }
    // Try loading from disk
    match sentinel_infrastructure::proof_store::load_chain(&session_id) {
        Ok(Some(chain)) => Json(serde_json::to_value(&chain).unwrap_or_default()),
        _ => Json(serde_json::json!({ "error": "Chain not found" })),
    }
}

async fn verify_chain(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Json<serde_json::Value> {
    let session = state.session.read().await;
    for (_skill, chain) in &session.proof_chains {
        if chain.session_id == session_id {
            let verification = chain.verify();
            return Json(serde_json::to_value(&verification).unwrap_or_default());
        }
    }
    Json(serde_json::json!({ "error": "Chain not found" }))
}
