//! Memory Debug API Endpoints
//!
//! GET /api/memories/status    — current memory injection state
//! GET /api/memories/precomputed — raw precomputed search results
//! GET /api/memories/injected   — last injected memories with IDs/scores

use axum::{http::StatusCode, routing::get, Json, Router};
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;

use super::{operational_read_audit, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/precomputed", get(precomputed))
        .route("/injected", get(injected))
}

/// Read and parse a JSON state file.
fn read_state_file_value(filename: &str) -> Result<Option<serde_json::Value>, String> {
    let path = sentinel_infrastructure::state_store::state_dir().join(filename);

    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content)
            .map(Some)
            .map_err(|e| format!("failed to parse {}: {e}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("failed to read {}: {err}", path.display())),
    }
}

fn state_file_envelope(filename: &str) -> serde_json::Value {
    match read_state_file_value(filename) {
        Ok(Some(value)) => serde_json::json!({
            "state_file": filename,
            "present": true,
            "value": value,
        }),
        Ok(None) => serde_json::json!({
            "state_file": filename,
            "present": false,
            "value": serde_json::Value::Null,
        }),
        Err(error) => serde_json::json!({
            "state_file": filename,
            "present": true,
            "value": serde_json::Value::Null,
            "error": error,
        }),
    }
}

/// GET /api/memories/status — overview of memory injection state.
async fn status() -> Result<Json<serde_json::Value>, StatusCode> {
    let precomputed = state_file_envelope("precomputed-memories.json");
    let injected = state_file_envelope("last-injected-memories.json");
    let config_exists = sentinel_infrastructure::qdrant::QdrantConfig::load().is_some();

    operational_json(
        OperationalApiReadSurface::MemoryStatus,
        serde_json::json!({
            "qdrant_configured": config_exists,
            "precomputed": precomputed,
            "last_injected": injected,
        }),
    )
    .await
}

/// GET /api/memories/precomputed — audited precomputed search state file.
async fn precomputed() -> Result<Json<serde_json::Value>, StatusCode> {
    operational_json(
        OperationalApiReadSurface::MemoryPrecomputed,
        state_file_envelope("precomputed-memories.json"),
    )
    .await
}

/// GET /api/memories/injected — audited last injected memory state file.
async fn injected() -> Result<Json<serde_json::Value>, StatusCode> {
    operational_json(
        OperationalApiReadSurface::MemoryInjected,
        state_file_envelope("last-injected-memories.json"),
    )
    .await
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
                "memory debug API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        previous_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_sentinel_home(path: &std::path::Path) -> Self {
            let previous_home = std::env::var_os("SENTINEL_HOME");
            std::env::set_var("SENTINEL_HOME", path);
            Self { previous_home }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("SENTINEL_HOME", value),
                None => std::env::remove_var("SENTINEL_HOME"),
            }
        }
    }

    #[test]
    fn state_file_envelope_distinguishes_missing_from_malformed_json() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());
        let state_dir = sentinel_infrastructure::state_store::state_dir();
        std::fs::create_dir_all(&state_dir).expect("state dir");

        let missing = state_file_envelope("precomputed-memories.json");
        assert_eq!(missing["present"], false);
        assert!(missing.get("error").is_none());

        std::fs::write(state_dir.join("precomputed-memories.json"), "{not-json")
            .expect("malformed memory state");
        let malformed = state_file_envelope("precomputed-memories.json");
        assert_eq!(malformed["present"], true);
        assert!(malformed["value"].is_null());
        assert!(malformed["error"]
            .as_str()
            .is_some_and(|error| error.contains("failed to parse")));
    }
}
