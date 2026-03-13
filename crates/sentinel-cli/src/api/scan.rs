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
    routing::{get, post},
    Json, Router,
};

use sentinel_application::scanner::{self, MarketplaceSnapshot};

use super::AppState;

/// Cache TTL — 5 seconds (matches Node.js dashboard behavior).
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
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
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
async fn scan() -> Json<MarketplaceSnapshot> {
    Json(get_snapshot())
}

/// GET /api/validation — validation results only
async fn validation() -> Json<serde_json::Value> {
    let snapshot = get_snapshot();
    Json(serde_json::to_value(&snapshot.validation).unwrap_or_default())
}

/// GET /api/counts — component counts only
async fn counts() -> Json<serde_json::Value> {
    let snapshot = get_snapshot();
    Json(serde_json::to_value(&snapshot.counts).unwrap_or_default())
}

/// POST /api/rescan — bust cache and rescan
async fn rescan() -> Json<MarketplaceSnapshot> {
    {
        let mut cache = CACHE.lock().unwrap();
        cache.snapshot = None;
        cache.last_scan = None;
    }
    Json(get_snapshot())
}
