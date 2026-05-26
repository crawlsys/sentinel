//! Memory Daemon Proxy API (Phase 8.e)
//!
//! Thin proxy to the `memory daemon` HTTP server (default `127.0.0.1:3011`).
//! The sentinel dashboard Memory pane hits these endpoints instead of
//! spawning its own Qdrant client, so auth + config live in one place.
//!
//! Endpoints:
//!   * `GET /api/memory/stats[?project=X]` — proxies the daemon's `/stats`.
//!   * `GET /api/memory/health`             — proxies the daemon's `/health`.
//!
//! If the daemon is unreachable (not running, firewalled, slow), we return a
//! 503 with a JSON error body so the dashboard can render a "daemon down"
//! pane without crashing. Timeout is 3 s per request.
//!
//! NOTE: a separate `memories.rs` module already exposes `/api/memories/*`
//! for the *legacy* memory injection state files (precomputed search,
//! last-injected list). This module is the new atom-store oriented proxy —
//! the `/api/memories` prefix stays for backcompat, `/api/memory` is the
//! single-entity form for the daemon.

use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use super::AppState;

/// The memory daemon base URL. Overridable via `SENTINEL_MEMORY_DAEMON_URL`
/// so the sentinel dashboard can point at a non-default daemon (alt port,
/// remote tailscale box, etc.) without rebuilding.
const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:3011";
const DAEMON_URL_ENV: &str = "SENTINEL_MEMORY_DAEMON_URL";

/// Per-request timeout. 3 s matches the spec for Phase 8.e.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Shared reqwest client — reqwest pools connections internally, and the
/// default client is cheap to clone. We lazy-init on first use so a sentinel
/// install that never touches the memory pane pays zero cost.
static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn client() -> &'static reqwest::Client {
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("build reqwest client with timeout")
    })
}

fn daemon_url() -> String {
    std::env::var(DAEMON_URL_ENV).unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_string())
}

#[derive(Debug, Deserialize)]
struct StatsQuery {
    project: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stats", get(stats_proxy))
        .route("/health", get(health_proxy))
}

/// GET /api/memory/stats[?project=X] — proxy the daemon's /stats.
async fn stats_proxy(Query(q): Query<StatsQuery>) -> Response {
    let base = daemon_url();
    let url = format!("{}/stats", base.trim_end_matches('/'));

    let mut req = client().get(&url);
    if let Some(ref p) = q.project {
        req = req.query(&[("project", p.as_str())]);
    }

    match req.send().await {
        Ok(resp) => forward_json(resp, &base).await,
        Err(e) => daemon_unavailable(&base, &e.to_string()),
    }
}

/// GET /api/memory/health — proxy the daemon's /health.
async fn health_proxy() -> Response {
    let base = daemon_url();
    let url = format!("{}/health", base.trim_end_matches('/'));
    match client().get(&url).send().await {
        Ok(resp) => forward_json(resp, &base).await,
        Err(e) => daemon_unavailable(&base, &e.to_string()),
    }
}

/// Forward a daemon response to the dashboard client. Preserves status code
/// when possible; on upstream error status, wraps in a structured envelope.
async fn forward_json(resp: reqwest::Response, base: &str) -> Response {
    let status = resp.status();
    // Map upstream status to axum. If parsing the body fails, surface that
    // as a 502 — the daemon gave us something, just not JSON we understand.
    let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("failed to read memory daemon response body: {e}"),
                    "daemon_url": base,
                })),
            )
                .into_response();
        }
    };

    // Try to parse as JSON first so we can surface it cleanly in the
    // dashboard; if the daemon emitted non-JSON (unlikely for Phase 8.e but
    // Prometheus metrics are text, and this module proxies only JSON
    // endpoints), fall back to a 502 envelope.
    let parsed: serde_json::Value = match serde_json::from_str(&body_text) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": format!("memory daemon returned non-JSON body: {e}"),
                    "daemon_url": base,
                })),
            )
                .into_response();
        }
    };

    let out_status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    (out_status, Json(parsed)).into_response()
}

/// Return a structured 503 when the daemon is unreachable (refused, timed
/// out, DNS). Phase 8.e spec: the dashboard should see a typed error, not a
/// stack trace.
fn daemon_unavailable(base: &str, reason: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": format!("memory daemon unavailable at {base}"),
            "reason": reason,
            "hint": "Start the daemon with: memory daemon",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes every test in this module that mutates
    /// `SENTINEL_MEMORY_DAEMON_URL`. Cargo runs tests in parallel by default,
    /// and the previous "the only env-touching test" comment was wrong —
    /// `daemon_url_uses_env_override_when_set`, `daemon_url_falls_back_to_default_when_unset`,
    /// and `daemon_unreachable_returns_503` all read+mutate the same env
    /// var, so without a shared mutex they race and intermittently see
    /// each other's writes (the fall-back test would observe the override
    /// test's value before its own `remove_var` ran). Holding the mutex
    /// across both the mutation AND the `daemon_url()` read closes the
    /// window completely.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the env lock, tolerating poisoning (a panic in another
    /// test holding the lock leaves it poisoned, but the env state itself
    /// is still recoverable from the saved `prev`).
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn default_daemon_url_is_3011() {
        // Constant pinned — if this ever changes, the memory repo daemon
        // default needs to flip in lockstep.
        assert_eq!(DEFAULT_DAEMON_URL, "http://127.0.0.1:3011");
    }

    #[test]
    fn daemon_url_uses_env_override_when_set() {
        let _guard = lock_env();
        let key = DAEMON_URL_ENV;
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "http://10.0.0.7:3099");
        assert_eq!(daemon_url(), "http://10.0.0.7:3099");
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn daemon_url_falls_back_to_default_when_unset() {
        let _guard = lock_env();
        let key = DAEMON_URL_ENV;
        let prev = std::env::var(key).ok();
        std::env::remove_var(key);
        assert_eq!(daemon_url(), DEFAULT_DAEMON_URL);
        if let Some(v) = prev {
            std::env::set_var(key, v);
        }
    }

    #[tokio::test]
    async fn daemon_unreachable_returns_503() {
        // Point at a port that is almost certainly closed. reqwest will
        // fail fast with ConnectionRefused, so we get the 503 without
        // waiting for the 3s timeout.
        let _guard = lock_env();
        let key = DAEMON_URL_ENV;
        let prev = std::env::var(key).ok();
        std::env::set_var(key, "http://127.0.0.1:1");

        let resp = health_proxy().await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
