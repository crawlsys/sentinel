use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde::Deserialize;
use tower_http::cors::{Any, CorsLayer};

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
    pub naming: NamingState,
    pub summary: SummaryState,
    /// Snapshot cache keyed by opts. Avoids re-scanning 90k events
    /// on every refresh when the store hasn't advanced. Holds at most
    /// a few entries — one per unique (limit, since_secs, include_hooks).
    pub cache: RwLock<Vec<CacheEntry>>,
    /// `/api/activity` cache, keyed by (sid, at_ts, window, limit).
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
    /// Kept for the SSE path, which still emits the GraphResponse
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
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    axum::Router::new()
        .route("/api/healthz", get(health::healthz))
        .route("/api/graph", get(graph_endpoint))
        .route("/api/activity/{session_id}", get(activity_endpoint))
        .route("/api/name-session/{session_id}", get(name_session_endpoint))
        .route("/api/summary/{session_id}", get(summary_endpoint))
        .route("/api/stream", get(sse::stream))
        .layer(cors)
        .with_state(state)
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
    let kind = SummaryKind::from_str(q.kind.as_deref().unwrap_or("card"))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "kind must be card|wait|narrative".into()))?;
    let r = summary::summarize(&state.summary, &session_id, kind, q.at_ts.as_deref()).await;
    Ok(axum::Json(r))
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
) -> Result<axum::response::Response, (StatusCode, String)> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let limit = q.limit.unwrap_or(state.window_limit);
    let since_secs = match q.since_secs {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some(6 * 3600),
    };
    let include_hooks = q.include_hooks.unwrap_or(false);
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
    let cached_body: Option<Arc<Vec<u8>>> = {
        let cache = state.cache.read().unwrap();
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

    let g = graph::load_graph_with(
        &conn,
        GraphOpts { limit, since_secs, include_hooks },
    )
    .map_err(internal)?;
    let body = serde_json::to_vec(&g).map_err(|e| internal(anyhow::anyhow!(e)))?;
    let body_arc = Arc::new(body);
    let g_arc = Arc::new(g);
    {
        let mut cache = state.cache.write().unwrap();
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
    use axum::http::header;
    use axum::response::IntoResponse;

    let limit = q.limit.unwrap_or(80);
    let window = q.window.unwrap_or(30);
    let key = (session_id.clone(), q.at_ts.clone(), window, limit);

    // TTL cache.
    {
        let cache = state.activity_cache.read().unwrap();
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
        let mut cache = state.activity_cache.write().unwrap();
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
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Build a 200 response carrying an empty `GraphResponse` with the
/// `error` field populated so the viewer can render a friendly
/// message instead of a hard 5xx.
fn degraded_response(msg: &str) -> axum::response::Response {
    use axum::http::header;
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
