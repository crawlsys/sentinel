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
            loop {
                let conn = match db::open_ro(&state.db_path) {
                    Ok(c) => c,
                    Err(_) => {
                        time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                };
                let cur = db::peek_max_seq(&conn).unwrap_or(0);
                if cur != last_seq {
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
                            if let Ok(mut cache) = state.cache.write() {
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
