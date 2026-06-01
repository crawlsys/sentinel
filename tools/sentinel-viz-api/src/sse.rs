use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use futures::stream::{self, Stream};
use tokio::time;

use crate::db;
use crate::graph::{self, GraphOpts};
use crate::server::{AppState, CacheEntry};

/// `/api/stream` — 250ms `MAX(seq)` probe. Emits a full graph snapshot
/// whenever the store advances; emits a keep-alive comment otherwise.
/// Reuses the cache shared with `/api/graph` so repeated cold requests
/// don't pay the ~7s build cost.
pub async fn stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let st = stream::unfold(
        (state, -1_i64),
        |(state, mut last_seq)| async move {
            // First poll: seed the opening frame from the shared graph
            // cache when a snapshot already exists, instead of forcing a
            // cold ~7s build for every new subscriber. The cache is keyed
            // identically to the default `/api/graph` request, so a warm
            // dashboard tab will usually have populated it.
            if last_seq < 0 {
                let key = (state.window_limit, Some(6 * 3600_i64), false);
                let seed = {
                    let cache = state.cache.read().unwrap_or_else(std::sync::PoisonError::into_inner);
                    cache
                        .iter()
                        .find(|e| e.key == key)
                        .map(|e| (Arc::clone(&e.body), e.last_seq))
                };
                if let Some((body, seq)) = seed {
                    last_seq = seq;
                    let payload = String::from_utf8((*body).clone()).unwrap_or_default();
                    return Some((Ok(Event::default().data(payload)), (state, last_seq)));
                }
            }
            loop {
                let conn = if let Ok(c) = db::open_ro(&state.db_path) { c } else {
                    // Signal degraded state to the client rather than
                    // silently looping. The UI can surface this the
                    // same way it treats a degraded /api/graph payload.
                    time::sleep(Duration::from_millis(500)).await;
                    return Some((
                        Ok(Event::default().comment("db-unavailable")),
                        (state, last_seq),
                    ));
                };
                let cur = db::peek_max_seq(&conn).unwrap_or(0);
                // `seq` is a monotonically increasing counter; only treat
                // a strictly higher value as new work. `!=` would also
                // fire on a transient lower read.
                if cur > last_seq {
                    last_seq = cur;
                    let opts = GraphOpts {
                        limit: state.window_limit,
                        since_secs: Some(6 * 3600),
                        include_hooks: false,
                        focused_session: None,
                    };
                    let key = (opts.limit, opts.since_secs, opts.include_hooks);
                    match graph::load_graph_with(&conn, opts) {
                        Ok(g) => {
                            let body = serde_json::to_vec(&g).unwrap_or_default();
                            let body_arc = Arc::new(body);
                            let g_arc = Arc::new(g);
                            {
                                let mut cache =
                                    state.cache.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                                cache.retain(|e| e.key != key);
                                cache.push(CacheEntry {
                                    key,
                                    last_seq: cur,
                                    body: Arc::clone(&body_arc),
                                    graph: Arc::clone(&g_arc),
                                });
                                if cache.len() > 8 {
                                    let drop_n = cache.len() - 8;
                                    cache.drain(0..drop_n);
                                }
                            }
                            let payload = String::from_utf8((*body_arc).clone())
                                .unwrap_or_default();
                            return Some((Ok(Event::default().data(payload)), (state, last_seq)));
                        }
                        Err(_) => {
                            return Some((Ok(Event::default().comment("error")), (state, last_seq)));
                        }
                    }
                }
                time::sleep(Duration::from_millis(250)).await;
            }
        },
    );
    Sse::new(st).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
