//! Memory Debug API Endpoints
//!
//! GET /api/memories/status    — current memory injection state
//! GET /api/memories/precomputed — raw precomputed search results
//! GET /api/memories/injected   — last injected memories with IDs/scores

use axum::{routing::get, Json, Router};

use super::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/precomputed", get(precomputed))
        .route("/injected", get(injected))
}

/// Read and parse a JSON state file, returning `null` if missing or invalid.
fn read_state_file(filename: &str) -> serde_json::Value {
    let path = match dirs::home_dir() {
        Some(h) => h
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join(filename),
        None => return serde_json::Value::Null,
    };

    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or(serde_json::Value::Null),
        Err(_) => serde_json::Value::Null,
    }
}

/// GET /api/memories/status — overview of memory injection state.
async fn status() -> Json<serde_json::Value> {
    let precomputed = read_state_file("precomputed-memories.json");
    let injected = read_state_file("last-injected-memories.json");

    let precomputed_query = precomputed
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let precomputed_ts = precomputed
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let precomputed_count = precomputed
        .get("results")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let injected_ts = injected
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let injected_count = injected
        .get("memories")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let injected_prompt = injected
        .get("user_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Check freshness
    let fresh = if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&precomputed_ts) {
        let age = (chrono::Utc::now() - ts.with_timezone(&chrono::Utc)).num_seconds();
        age <= 300
    } else {
        false
    };

    // Check config
    let config_exists = dirs::home_dir()
        .map(|h| h.join(".qdrant").join("config.json").exists())
        .unwrap_or(false);

    Json(serde_json::json!({
        "qdrant_configured": config_exists,
        "precomputed": {
            "query": precomputed_query,
            "timestamp": precomputed_ts,
            "hit_count": precomputed_count,
            "fresh": fresh,
        },
        "last_injected": {
            "timestamp": injected_ts,
            "hit_count": injected_count,
            "user_prompt": injected_prompt,
        },
    }))
}

/// GET /api/memories/precomputed — raw precomputed search results.
async fn precomputed() -> Json<serde_json::Value> {
    Json(read_state_file("precomputed-memories.json"))
}

/// GET /api/memories/injected — last injected memory entries with IDs and scores.
async fn injected() -> Json<serde_json::Value> {
    Json(read_state_file("last-injected-memories.json"))
}
