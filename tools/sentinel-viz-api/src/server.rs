use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::routing::get;
use serde::Deserialize;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::activity;
use crate::db;
use crate::graph::{self, GraphOpts};
use crate::health;
use crate::model::{ActivityResponse, GraphResponse};
use crate::naming::{self, NamingState};
use crate::sse;
use crate::summary::{self, SummaryKind, SummaryState};

pub struct AppState {
    pub db_path: PathBuf,
    pub window_limit: usize,
    pub started_at: Instant,
    /// Whether `POST /api/config` is allowed to mutate the runtime
    /// model config. Set from the bind host in `main`: writes are
    /// permitted only when the server is bound to a loopback address.
    /// A server reachable from the network must not let arbitrary
    /// callers register an `OpenAI` key or repoint the Ollama URL.
    pub allow_config_writes: bool,
    pub naming: NamingState,
    pub summary: SummaryState,
    /// Snapshot cache keyed by opts. Avoids re-scanning 90k events
    /// on every refresh when the store hasn't advanced. Holds at most
    /// a few entries — one per unique (limit, `since_secs`, `include_hooks`).
    pub cache: RwLock<Vec<CacheEntry>>,
    /// `/api/activity` cache, keyed by (sid, `at_ts`, window, limit).
    /// Short TTL — transcript JSONLs are small and the bridge appends
    /// to them out-of-band, so we expire eagerly.
    pub activity_cache: RwLock<Vec<ActivityCacheEntry>>,
}

pub struct CacheEntry {
    pub key: (usize, Option<i64>, bool),
    pub last_seq: i64,
    /// Pre-serialised JSON body — cheap to ship from the handler
    /// without re-serialising on every hit.
    pub body: Arc<Vec<u8>>,
    /// Kept for the SSE path, which still emits the `GraphResponse`
    /// directly so it can be re-serialised with `data: ` framing.
    pub graph: Arc<GraphResponse>,
}

pub struct ActivityCacheEntry {
    pub key: (String, Option<String>, i64, usize),
    pub built_at: Instant,
    pub body: Arc<Vec<u8>>,
}

const ACTIVITY_TTL_SECS: u64 = 6;

pub fn router(state: Arc<AppState>) -> axum::Router {
    // Lock CORS down to the known frontend origin(s). The server
    // proxies a user-supplied OpenAI key and can be bound to a
    // non-loopback host, so reflecting `Any` would let any page on
    // the operator's machine drive it. Methods and headers are
    // limited to what the API actually uses (GET/POST + JSON bodies).
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(allowed_origins()))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE]);
    axum::Router::new()
        .route("/api/healthz", get(health::healthz))
        .route("/api/graph", get(graph_endpoint))
        .route("/api/activity/{session_id}", get(activity_endpoint))
        .route("/api/name-session/{session_id}", get(name_session_endpoint))
        .route("/api/summary/{session_id}", get(summary_endpoint))
        .route("/api/config", get(get_config).post(set_config))
        .route("/api/kpis", get(kpis_endpoint))
        .route("/api/stream", get(sse::stream))
        .layer(cors)
        .with_state(state)
}

/// CORS allowlist. Reads `SENTINEL_VIZ_ALLOWED_ORIGINS` (comma-separated
/// absolute origins, e.g. `http://localhost:8083,https://viz.example`)
/// and defaults to the Next.js dev origin on both loopback spellings.
/// Unparseable entries are dropped with a warning rather than widening
/// the policy.
fn allowed_origins() -> Vec<HeaderValue> {
    const DEFAULTS: [&str; 2] = ["http://localhost:8083", "http://127.0.0.1:8083"];
    let raw = std::env::var("SENTINEL_VIZ_ALLOWED_ORIGINS").unwrap_or_default();
    let configured: Vec<HeaderValue> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("ignoring invalid CORS origin '{o}': {e}");
                None
            }
        })
        .collect();
    if configured.is_empty() {
        DEFAULTS
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect()
    } else {
        configured
    }
}

/// `true` when a bind host names a loopback interface: `localhost`,
/// `::1` (optionally bracketed), or any address in `127.0.0.0/8`.
/// Used to decide whether `POST /api/config` mutations are safe.
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // Accept a bracketed IPv6 literal as `main` may pass `[::1]`.
    let unbracketed = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    unbracketed
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

async fn name_session_endpoint(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> axum::Json<naming::NameResponse> {
    axum::Json(naming::name_session(&state.naming, &session_id).await)
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    pub kind: Option<String>,
    pub at_ts: Option<String>,
}

async fn summary_endpoint(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Query(q): Query<SummaryQuery>,
) -> Result<axum::Json<summary::SummaryResponse>, (StatusCode, String)> {
    let kind = SummaryKind::parse(q.kind.as_deref().unwrap_or("card"))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "kind must be card|wait|narrative".into()))?;
    let r = summary::summarize(&state.summary, &session_id, kind, q.at_ts.as_deref()).await;
    Ok(axum::Json(r))
}

#[derive(serde::Serialize)]
pub struct ConfigResponse {
    /// Active model label e.g. "openai:gpt-4o-mini", "local:qwen2.5:1.5b", or "none".
    pub model: String,
    /// Whether an API key (for `OpenAI`) is bound. We never echo the key.
    pub has_key: bool,
}

async fn get_config(State(state): State<Arc<AppState>>) -> axum::Json<ConfigResponse> {
    let m = state.naming.model.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (model, has_key) = match m.as_ref() {
        None => ("none".to_string(), false),
        Some(crate::llm::ModelConfig::OpenAi { model, api_key }) => {
            (format!("openai:{model}"), !api_key.is_empty())
        }
        Some(crate::llm::ModelConfig::LocalOllama { model, .. }) => {
            (format!("local:{model}"), true)
        }
    };
    axum::Json(ConfigResponse { model, has_key })
}

#[derive(serde::Deserialize)]
pub struct SetConfigBody {
    /// "none" | "openai:<model>" | "local:<model>".
    pub model: String,
    /// `OpenAI` API key, only used when model is "openai:*".
    pub openai_api_key: Option<String>,
    /// Optional Ollama URL override (defaults to existing or <http://127.0.0.1:11434>).
    pub ollama_url: Option<String>,
}

async fn kpis_endpoint(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<crate::kpis::Kpis>, (StatusCode, String)> {
    // Reuse the cached default graph snapshot when present to avoid
    // rebuilding 90k events for each KPI poll.
    let cur_seq = db::open_ro(&state.db_path)
        .ok()
        .and_then(|c| db::peek_max_seq(&c).ok())
        .unwrap_or(0);
    let key = (state.window_limit, Some(6 * 3600_i64), false);
    let cached_graph = {
        let cache = state.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache
            .iter()
            .find(|e| e.key == key && e.last_seq == cur_seq)
            .map(|e| Arc::clone(&e.graph))
    };
    let graph_arc: Arc<crate::model::GraphResponse> = if let Some(g) = cached_graph {
        g
    } else {
        let conn = db::open_ro(&state.db_path).map_err(internal)?;
        let g = graph::load_graph_with(&conn, GraphOpts::default()).map_err(internal)?;
        Arc::new(g)
    };
    Ok(axum::Json(crate::kpis::compute(&graph_arc)))
}

async fn set_config(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SetConfigBody>,
) -> Result<axum::Json<ConfigResponse>, (StatusCode, String)> {
    use crate::llm::ModelConfig;
    // Refuse runtime config writes when reachable off-loopback — this
    // endpoint binds a user-supplied OpenAI key and Ollama URL into
    // server state, which must not be writable by remote callers.
    if !state.allow_config_writes {
        return Err((
            StatusCode::FORBIDDEN,
            "config writes are disabled when the server is bound to a non-loopback host".into(),
        ));
    }
    let parsed: Option<ModelConfig> = match body.model.as_str() {
        "" | "none" => None,
        s if s.starts_with("openai:") => {
            let model = s.trim_start_matches("openai:").to_string();
            let key = body.openai_api_key.unwrap_or_default();
            if key.is_empty() {
                return Err((StatusCode::BAD_REQUEST, "openai_api_key required for openai:*".into()));
            }
            Some(ModelConfig::OpenAi { model, api_key: key })
        }
        s if s.starts_with("local:") => {
            let model = s.trim_start_matches("local:").to_string();
            let base = body.ollama_url
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| std::env::var("OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into()));
            // SSRF guard: the server POSTs to `{base}/api/generate`, so
            // a loopback-only host (or an explicit allowlist entry) is
            // required before we accept the value.
            crate::llm::validate_ollama_url(&base)
                .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            Some(ModelConfig::LocalOllama { model, base_url: base })
        }
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("model must be 'none', 'openai:*', or 'local:*' — got '{other}'"),
            ));
        }
    };
    state.naming.set_model(parsed.clone());
    state.summary.set_model(parsed);
    Ok(get_config(State(state)).await)
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
    /// Session id (`data.session_id`) that should get the larger
    /// per-session window. Others use the default cap.
    pub focused_session: Option<String>,
}

async fn graph_endpoint(
    State(state): State<Arc<AppState>>,
    Query(q): Query<GraphQuery>,
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::response::IntoResponse;

    let limit = q.limit.unwrap_or(state.window_limit);
    let since_secs = match q.since_secs {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(6 * 3600),
    };
    let include_hooks = q.include_hooks.unwrap_or(false);
    let focused_session = q.focused_session.filter(|s| !s.is_empty());
    let key = (limit, since_secs, include_hooks);

    // If we can't even open the DB, return a degraded-but-valid
    // GraphResponse so the viewer can surface a friendly error
    // instead of a raw 500.
    let conn = match db::open_ro(&state.db_path) {
        Ok(c) => c,
        Err(e) => {
            return Ok(degraded_response(&format!(
                "cannot open sentinel.db: {e}"
            )));
        }
    };
    let cur_seq = db::peek_max_seq(&conn).unwrap_or(0);
    // Skip the shared cache when a focused_session is set — that
    // request varies per session. Trade-off: pay the rebuild cost
    // on each focus change.
    if focused_session.is_none() {
        let cached_body: Option<Arc<Vec<u8>>> = {
            let cache = state.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
            cache
                .iter()
                .find(|e| e.key == key && e.last_seq == cur_seq)
                .map(|e| Arc::clone(&e.body))
        };
        if let Some(body) = cached_body {
            return Ok((
                [(header::CONTENT_TYPE, "application/json")],
                (*body).clone(),
            )
                .into_response());
        }
    }

    let g = graph::load_graph_with(
        &conn,
        GraphOpts {
            limit,
            since_secs,
            include_hooks,
            focused_session: focused_session.clone(),
        },
    )
    .map_err(internal)?;
    let body = serde_json::to_vec(&g).map_err(|e| internal(anyhow::anyhow!(e)))?;
    let body_arc = Arc::new(body);
    let g_arc = Arc::new(g);
    if focused_session.is_none() {
        let mut cache = state.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|e| e.key != key);
        cache.push(CacheEntry {
            key,
            last_seq: cur_seq,
            body: Arc::clone(&body_arc),
            graph: Arc::clone(&g_arc),
        });
        if cache.len() > 8 {
            let drop_n = cache.len() - 8;
            cache.drain(0..drop_n);
        }
    }
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        (*body_arc).clone(),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct ActivityQuery {
    pub limit: Option<usize>,
    pub at_ts: Option<String>,
    pub window: Option<i64>,
}

async fn activity_endpoint(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
    Query(q): Query<ActivityQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let limit = q.limit.unwrap_or(80);
    let window = q.window.unwrap_or(30);
    let key = (session_id.clone(), q.at_ts.clone(), window, limit);

    // TTL cache.
    {
        let cache = state.activity_cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        for e in cache.iter() {
            if e.key == key && e.built_at.elapsed().as_secs() < ACTIVITY_TTL_SECS {
                return (
                    [(header::CONTENT_TYPE, "application/json")],
                    (*e.body).clone(),
                )
                    .into_response();
            }
        }
    }

    let a: ActivityResponse =
        activity::session_activity(&session_id, limit, q.at_ts.as_deref(), window);
    let body = serde_json::to_vec(&a).unwrap_or_default();
    let body_arc = Arc::new(body);
    {
        let mut cache = state.activity_cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.retain(|e| {
            e.key != key && e.built_at.elapsed().as_secs() < ACTIVITY_TTL_SECS * 4
        });
        cache.push(ActivityCacheEntry {
            key,
            built_at: Instant::now(),
            body: Arc::clone(&body_arc),
        });
        if cache.len() > 32 {
            let drop_n = cache.len() - 32;
            cache.drain(0..drop_n);
        }
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        (*body_arc).clone(),
    )
        .into_response()
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    // Log the full error (incl. context like the DB path) server-side,
    // but never leak internal detail to the client.
    tracing::error!(error = ?e, "internal server error");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
}

/// Build a 200 response carrying an empty `GraphResponse` with the
/// `error` field populated so the viewer can render a friendly
/// message instead of a hard 5xx.
fn degraded_response(msg: &str) -> axum::response::Response {
    use axum::response::IntoResponse;

    let g = crate::model::GraphResponse {
        nodes: vec![],
        edges: vec![],
        events: vec![],
        max_seq: 0,
        window_limit: 0,
        stats: crate::model::GraphStats::default(),
        error: Some(msg.to_string()),
    };
    let body = serde_json::to_vec(&g).unwrap_or_default();
    (
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt; // for `oneshot`

    fn state_with_writes(allow: bool) -> Arc<AppState> {
        Arc::new(AppState {
            db_path: PathBuf::from("/nonexistent/sentinel.db"),
            window_limit: 100,
            started_at: Instant::now(),
            allow_config_writes: allow,
            cache: RwLock::new(Vec::new()),
            activity_cache: RwLock::new(Vec::new()),
            naming: NamingState::from_env(),
            summary: SummaryState::from_env(),
        })
    }

    #[test]
    fn loopback_hosts_are_recognised() {
        for h in ["127.0.0.1", "localhost", "LOCALHOST", "::1", "[::1]", "127.0.0.9", " 127.0.0.1 "] {
            assert!(is_loopback_host(h), "should be loopback: {h:?}");
        }
        for h in ["0.0.0.0", "192.168.1.4", "example.com", "::", "10.0.0.1"] {
            assert!(!is_loopback_host(h), "should not be loopback: {h:?}");
        }
    }

    #[tokio::test]
    async fn post_config_is_forbidden_off_loopback() {
        let app = router(state_with_writes(false));
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"model":"none"}"#))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_config_rejects_external_ollama_url_on_loopback() {
        // Writes are permitted (loopback), but the SSRF guard must
        // still reject a non-loopback Ollama URL with a 400.
        let app = router(state_with_writes(true));
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/config")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"local:qwen2.5","ollama_url":"http://169.254.169.254"}"#,
            ))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
