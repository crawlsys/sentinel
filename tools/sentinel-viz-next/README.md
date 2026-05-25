# sentinel-viz-next

Next.js 16 / React 19 viewer for the Sentinel activity graph.

Replaces the inline-HTML viewer baked into the Python
`tools/sentinel-viz/viz_server.py`. The Python server keeps running on
`:8081` until this stack reaches behavioural parity and the user kills
it. See `~/.agents/plans/sentinel-viz-next.md`.

## Components

- `components/GraphCanvas.tsx` — SVG + `d3-force` simulation. Pan / zoom
  via `d3-zoom`. Click a node → `onSelectNode`.
- `components/EventTicker.tsx` — right-rail ticker. Groups consecutive
  events by `(session_id, type, tool_call_id, outcome)` (plan
  gotcha #9). Click a row → focus the referenced node.
- `components/PanelInspector.tsx` — middle-rail node inspector. Pulls
  `/api/activity/{session_id}` via TanStack Query, shows the last 8
  segments.
- `components/StatusBar.tsx` — top bar. Connection state, window /
  corpus counts, current `max_seq`.

## Run

The Rust API on `:8082` is canonical. The Next.js dev server reaches it
via `NEXT_PUBLIC_VIZ_API`:

```
# terminal 1
cd ../sentinel-viz-api && cargo run

# terminal 2
NEXT_PUBLIC_VIZ_API=http://127.0.0.1:8082 pnpm dev -p 8083
```

## Tests

```
pnpm test          # vitest — unit + component (happy-dom)
pnpm test:e2e      # playwright smoke (needs both servers running)
pnpm build         # full prod build + TypeScript check
```

Component specs live under `tests/components/`; pure utility specs
under `tests/lib/`; the single e2e smoke under `tests/e2e/`. The plan
treats vitest as the per-commit gate; playwright is a phase-exit gate.

## Data shapes

See `types/api.ts` — these are hand-mirrored from the Rust
`crates::model` types. If the Rust side changes a field, update both
sides in the same PR.
