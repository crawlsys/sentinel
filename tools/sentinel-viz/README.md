# sentinel-viz

A two-process live visualizer for [Sentinel](https://github.com/garysomerhalder/sentinel) workflow activity, backed by [activegraph](https://github.com/yoheinakajima/activegraph) for event-sourced storage.

```
┌──────────────────────────────┐         ┌──────────────────────────┐
│ sentinel-managed JSONL streams │   →   │ sentinel_bridge.py       │
│  ~/.claude/sentinel/metrics/   │   →   │ (tails JSONL, ingests    │
│  ~/.claude-sentinel/.../       │   →   │  into activegraph SQLite)│
└──────────────────────────────┘         └──────────┬───────────────┘
                                                    │
                                       sentinel.db (events table)
                                                    │
                                          ┌─────────▼──────────┐
                                          │ viz_server.py      │
                                          │  · /api/graph      │   ← polled / SSE
                                          │  · /api/stream     │   ← live push
                                          │  · /api/activity   │   ← transcript slice
                                          │  · /  (D3 viz UI)  │
                                          └────────────────────┘
                                                    │
                                              http://localhost:8081
```

## What's in the box

- **`sentinel_bridge.py`** — tails Sentinel's `hook-invocations.jsonl` + `sessions.jsonl` from *both* `~/.claude/sentinel/metrics/` (real) and `~/.claude-sentinel/sentinel/metrics/` (sandbox autonomous loops). Ingests into an [activegraph](https://github.com/yoheinakajima/activegraph) SQLite store as `SentinelSession`, `SentinelHookInvocation`, `has_invocation` relations, and `sentinel.hook_ingested` / `sentinel.session_started` / `sentinel.hook_denied` domain events. Resumable — re-runs skip already-seen trace_ids.
- **`viz_server.py`** — stdlib HTTP server that reads the activegraph SQLite event store and serves a live D3 force-directed visualization. Same store powers the underlying graph, a live ticker, an info panel with per-invocation transcript slices, and optional OpenAI summaries.

## Quick start

```bash
# From the sentinel repo root, the tool lives at tools/sentinel-viz/.
cd /path/to/sentinel/tools/sentinel-viz

# One-shot bring-up (idempotent — re-runs are no-ops if already alive).
./ensure-viz.sh

# Open the UI
xdg-open http://127.0.0.1:8081/   # or just hit it in a browser
```

`ensure-viz.sh` spawns both the bridge and the viz server in the background,
keeps them running across re-runs, and respects:
- `SENTINEL_VIZ_PORT` (default `8081`)
- `SENTINEL_VIZ_DISABLE=1` (skip entirely)
- `SENTINEL_VIZ_LOG_DIR` (default `/tmp`)

To auto-launch the viz from every Claude Code session, drop a hook into
`~/.claude/settings.json` (or the sandbox `~/.claude-sentinel/config/settings.json`):

```json
"hooks": {
  "SessionStart": [{
    "matcher": "",
    "hooks": [{
      "type": "command",
      "command": "/path/to/sentinel/tools/sentinel-viz/ensure-viz.sh",
      "async": true
    }]
  }]
}
```

A common pattern is to symlink the script onto `PATH` so the absolute path in
`settings.json` survives worktree moves:

```bash
ln -sf /path/to/sentinel/tools/sentinel-viz/ensure-viz.sh ~/.local/bin/sentinel-ensure-viz
```

### Manual bring-up (without ensure-viz.sh)

```bash
# 1. Bridge (live-tail mode — keeps SQLite store fresh)
nohup ~/.local/share/pipx/venvs/activegraph/bin/python \
  tools/sentinel-viz/sentinel_bridge.py --tail \
  > /tmp/sentinel-bridge.log 2>&1 &
disown

# 2. Viz server (port 8081)
nohup python3 tools/sentinel-viz/viz_server.py --port 8081 \
  > /tmp/sentinel-viz.log 2>&1 &
disown
```

## Viz features

- **Force-directed graph** of sessions and their hook invocations, with derived `next_in_session` chain edges so each session shows up as a path rather than a sunburst pile.
- **Window strategy:** top-K most recently-active sessions × N invocations each (defaults `K=6, N=40` — set in `load_graph()`). Stats panel shows `window N / corpus M` so you can tell at a glance how much of the total stream you're seeing.
- **Live updates via Server-Sent Events** (`/api/stream`) — server pushes whenever `max_seq` changes (250 ms tick). Auto-reconnects.
- **Active-session pulse** — any session/invocation with a hook timestamp in the last 30 s gets a CSS pulse animation plus an expanding ring. Decays naturally; re-rendered every 5 s.
- **Pan / zoom** — drag empty space to pan, scroll to zoom (0.2× – 4×), double-click empty to reset to identity. Node-drag still drags individual nodes.
- **Snap-to-node on click** — clicking a ticker row or graph node smoothly pans + zooms to center it; panel-aware (biases target leftward when panel is open).
- **Focus mode** — when a node is selected, other nodes/edges fade to ~12 % opacity; direct neighbors brighten; connected edges go accent-bright + thicker.
- **Right-edge slide-out panel** (~20 % width, 66 % height) — opens on ticker-card or node click. Sections: AI summary (button), session activity (transcript slice, ts-scoped to ±30 s for invocations), raw payload JSON, session chain.
- **Session activity feed** — for the selected node, reads the corresponding session's conversation jsonl from `~/.claude*/projects/.../<session_id>.jsonl` and shows the recent tool calls, user prompts, assistant text, and tool results with heuristic summaries (`Bash` → command, `Read`/`Write` → file path, `TaskUpdate` → compact JSON, etc.).
- **OpenAI summaries (optional)** — enter an OpenAI API key in the right-rail widget; key is stored in browser `localStorage` and never sent to the Python server. Model selector defaults to `gpt-4o-mini`. The panel's `[summarize]` button POSTs the activity slice directly to `api.openai.com/v1/chat/completions` and renders a 2-4 sentence summary.

## API

| Endpoint | Method | Returns |
|---|---|---|
| `/` | GET | The single-page UI (embedded HTML/CSS/JS) |
| `/api/graph` | GET | One-shot snapshot — `{nodes, edges, events, max_seq, window_limit, stats}` |
| `/api/stream` | GET | Server-Sent Events stream — same payload as `/api/graph`, pushed on `max_seq` change |
| `/api/activity/<session_id>` | GET | Transcript-derived activity stream. Query params: `limit` (default 80), `at_ts` (ISO timestamp; if present, returns only events within `±window` seconds), `window` (default 30) |

## Configuration knobs (edit in code)

| Where | Knob | Default | Effect |
|---|---|---|---|
| `viz_server.py` `load_graph()` | `K_SESSIONS` | 6 | How many recent sessions to include in the window |
| `viz_server.py` `load_graph()` | `PER_SESSION_CAP` | 40 | Per-session invocation cap |
| `viz_server.py` HTML | `PULSE_WINDOW_SECS` | 30 | Age threshold for active-node pulse |
| `viz_server.py` HTML | poll/SSE tick | 250 ms | Server-side change-detection cadence |
| `viz_server.py` JS | `panToNode` `scale` | 1.6 | Zoom level when snapping to a clicked node |
| `viz_server.py` JS | `_tool_summary()` | heuristic | Single function — swap for an Ollama / local-model call if you want richer summaries |

## Data model in activegraph

| Object | Fields |
|---|---|
| `SentinelSession` | session_id, cwd, platform, started_at |
| `SentinelHookInvocation` | hook, event, outcome, session_id, trace_id, duration_us, tool, repo_root, ts |

| Relation | Source → Target |
|---|---|
| `has_invocation` | SentinelSession → SentinelHookInvocation |
| `next_in_session` | SentinelHookInvocation → SentinelHookInvocation (derived; consecutive in session by ts) |

| Domain event | When emitted |
|---|---|
| `sentinel.session_started` | first time a session_id is seen |
| `sentinel.hook_ingested` | every hook invocation |
| `sentinel.hook_denied` | hook invocation with outcome=deny (ready for `@ag.behavior` subscribers) |

## What's parked for next iteration

1. **Local Ollama-backed summarizer** — currently uses heuristics in `_tool_summary()` and the AI summary path requires OpenAI. Easy swap: have `_tool_summary()` POST to `http://localhost:11434/api/generate` for `qwen2.5-1.5b` or similar.
2. **Causality edges from `caused_by`** — the activegraph events table has a `caused_by` column the bridge isn't populating. Filling it would give real causality arrows (which hook triggered which).
3. **TurnEvent grouping** — fan-out branching becomes visible if we group hooks that fire within a 100 ms window on the same session into a synthetic `TurnEvent` parent. Pure bridge change, ~30 lines.
4. **Cross-session edges from `Task`/`Agent` tool calls** — would visualize subagent delegation.
5. **Timeline scrubber** — bottom-of-canvas slider to filter the node window by ts range.
6. **Tool input/output viewer in panel** — full Bash command output, full Read file contents, etc., on demand.

## Where it lives

```
sentinel/tools/sentinel-viz/
├── README.md             ← this file
├── ensure-viz.sh         ← idempotent bring-up; safe from a SessionStart hook
├── sentinel_bridge.py    ← JSONL → activegraph SQLite ingester (live-tail)
├── viz_server.py         ← stdlib HTTP server + embedded D3 UI
├── static/
│   └── d3.v7.min.js      ← vendored D3 so the UI is offline-capable
└── sentinel.db           ← activegraph SQLite store (gitignored; written by bridge)
```

The viz reads the same SQLite the bridge writes; both can be run independently. Killing one doesn't break the other (the viz reads the DB in `?mode=ro` and tolerates the DB being absent).
