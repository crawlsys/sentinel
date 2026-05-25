use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use futures::stream::{self, Stream};
use tokio::time;

use crate::db;
use crate::graph;
use crate::server::AppState;

/// `/api/stream` — 250ms poll loop. Emits a full graph snapshot
/// whenever `MAX(seq)` advances; emits a comment keep-alive otherwise.
/// Matches the SSE behaviour of viz_server.py.
pub async fn stream(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let st = stream::unfold(
        (state, -1_i64),
        |(state, mut last_seq)| async move {
            loop {
                let cur = db::open_ro(&state.db_path)
                    .ok()
                    .and_then(|c| db::peek_max_seq(&c).ok())
                    .unwrap_or(-1);
                if cur != last_seq {
                    last_seq = cur;
                    match graph::load_graph_from_path(&state.db_path, state.window_limit) {
                        Ok(g) => {
                            let payload = serde_json::to_string(&g).unwrap_or_default();
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
