# sentinel-viz-api

Rust HTTP API that serves the Sentinel activity graph. Read-only
consumer of the SQLite store written by `tools/sentinel-viz/sentinel_bridge.py`.

Replaces the data layer of the Python `tools/sentinel-viz/viz_server.py`
during the `sentinel-viz-next` rewrite. Runs alongside the old server
(`:8081` vs `:8082`) until the cutover. See
`~/.agents/plans/sentinel-viz-next.md`.

## Endpoints

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/healthz` | `{ok, db_max_seq, uptime_sec}` |
| `GET` | `/api/graph?limit=N` | Full graph snapshot, parity with Python `load_graph()`. Default `limit=100`. |
| `GET` | `/api/activity/{session_id}?limit=N&at_ts=ISO&window=N` | Transcript-derived activity rolled up into segments. Same shape as Python `session_activity()`. |
| `GET` | `/api/stream` | SSE. 250ms `MAX(seq)` probe, emits a full graph snapshot on every change, keep-alive comment between. |

## Run

```
cargo run                       # binds 127.0.0.1:8082 by default
SENTINEL_VIZ_API_PORT=8092 cargo run
SENTINEL_VIZ_API_HOST=0.0.0.0 cargo run
SENTINEL_VIZ_DB=/some/other.sentinel.db cargo run
```

## Tests

```
cargo test
```

- `tests/db_read.rs` — smoke test against the real bridge store
  (skipped cleanly when no store exists).
- `tests/graph_fixture.rs` — builds a synthetic SQLite, asserts
  windowing + ticker expansion + session_status annotation.
- `tests/activity_fixture.rs` — builds a synthetic Claude transcript
  JSONL, asserts segment roll-up + tool_summary heuristics.

## Architecture

```
sentinel_bridge.py (Python)  →  sentinel.db (SQLite)
                                       ↑
                              sentinel-viz-api (Rust, this crate)
                                       ↑
                              sentinel-viz-next (Next.js, sibling dir)
```

Read-only on the SQLite side; the bridge owns all writes. The Rust
API never blocks on the bridge — both processes hold their own
`rusqlite::Connection` instances opened with `SQLITE_OPEN_READ_ONLY`.
