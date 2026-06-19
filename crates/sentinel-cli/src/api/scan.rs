//! Marketplace Scan API Endpoints
//!
//! GET  /api/scan       — full marketplace snapshot (5s cache)
//! GET  /api/validation — validation results only
//! GET  /api/counts     — component counts only
//! POST /api/rescan     — bust cache and rescan

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};

use sentinel_application::scanner::{self, MarketplaceSnapshot};
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;

use super::{operational_read_audit, AppState};

/// Cache TTL — 5 seconds (matches the prior Node.js scanner behavior).
const CACHE_TTL: Duration = Duration::from_secs(5);

/// In-memory cache for the marketplace snapshot.
struct ScanCache {
    snapshot: Option<MarketplaceSnapshot>,
    last_scan: Option<Instant>,
}

static CACHE: Mutex<ScanCache> = Mutex::new(ScanCache {
    snapshot: None,
    last_scan: None,
});

/// Get the marketplace root directory (~/.claude/).
fn marketplace_root() -> PathBuf {
    sentinel_infrastructure::paths::home_root_or_fatal().join(".claude")
}

/// Get a cached or fresh marketplace snapshot.
fn get_snapshot() -> MarketplaceSnapshot {
    let mut cache = CACHE.lock().unwrap();

    if let (Some(ref snapshot), Some(last)) = (&cache.snapshot, cache.last_scan) {
        if last.elapsed() < CACHE_TTL {
            return snapshot.clone();
        }
    }

    let snapshot = scanner::scan_marketplace(&marketplace_root());
    cache.snapshot = Some(snapshot.clone());
    cache.last_scan = Some(Instant::now());
    snapshot
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/scan", get(scan))
        .route("/validation", get(validation))
        .route("/counts", get(counts))
        .route("/rescan", post(rescan))
}

/// GET /api/scan — full marketplace snapshot
async fn scan() -> Result<Json<serde_json::Value>, StatusCode> {
    operational_json(
        OperationalApiReadSurface::Scan,
        serde_json::to_value(get_snapshot()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    )
    .await
}

/// GET /api/validation — validation results only
async fn validation() -> Result<Json<serde_json::Value>, StatusCode> {
    let snapshot = get_snapshot();
    operational_json(
        OperationalApiReadSurface::Validation,
        serde_json::to_value(&snapshot.validation)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    )
    .await
}

/// GET /api/counts — component counts only
async fn counts() -> Result<Json<serde_json::Value>, StatusCode> {
    let snapshot = get_snapshot();
    operational_json(
        OperationalApiReadSurface::Counts,
        serde_json::to_value(&snapshot.counts).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    )
    .await
}

/// POST /api/rescan — bust cache and rescan
async fn rescan() -> Result<Json<serde_json::Value>, StatusCode> {
    {
        let mut cache = CACHE.lock().unwrap();
        cache.snapshot = None;
        cache.last_scan = None;
    }
    operational_json(
        OperationalApiReadSurface::Rescan,
        serde_json::to_value(get_snapshot()).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
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
                "operational scan API read graph audit failed; refusing unaudited response"
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
    fn marketplace_root_uses_authoritative_home_root() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        assert_eq!(marketplace_root(), tmp.path().join(".claude"));
    }
}
