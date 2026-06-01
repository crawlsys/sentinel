# sentinel-viz-next — peel-off contract

> **WORKSTREAM: sentinel-viz** — this Next.js app is intended to be
> extractable from the Sentinel monorepo into its own repository
> (`sentinel-viz`) alongside `sentinel-viz-api/`, without rewriting
> any of its code.

Per user directive (2026-05-25):
> "ensure that work can easily be peeled off into a separate
> workstream when needed (via comments, graph, smoke signals, idgaf)"

## What's separable today

- `package.json` declares no internal Sentinel dependency. All deps
  resolve from npm.
- The app talks to **one** outside-the-app endpoint: the Rust API at
  `NEXT_PUBLIC_VIZ_API` (default `http://127.0.0.1:8082`). That's
  the entire cross-boundary surface.
- `types/api.ts` is the wire-format duplication of
  `tools/sentinel-viz-api/src/model.rs`. Hand-mirrored, by design —
  the Rust types are the source of truth. If you rename a field in
  Rust, rename it here too (look for `WORKSTREAM: sentinel-viz-api`
  tags marking the touchpoints).

## Tag pattern

Same as the Rust side. Grep `WORKSTREAM:` to find every
cross-boundary line. Today they appear in:

- `types/api.ts` — every interface mirrors a Rust struct
- `lib/api.ts` — the API base URL knob
- `lib/sse.ts` — the SSE endpoint URL

## How to extract

1. `git filter-repo --path tools/sentinel-viz-next --path tools/sentinel-viz-api` against a Sentinel clone.
2. Move both to a new repo (`sentinel-viz`). Keep them as
   sibling directories: `sentinel-viz/api/` and `sentinel-viz/web/`.
3. Update the Sentinel docker dev launcher's bind-mounts if you
   want the new repo bind-mounted into the container instead.

## Smoke signals if the boundary breaks

- Any new Next.js code that imports from `../../crates/*` (i.e.
  reaching INTO the Sentinel Rust tree). The Next build will fail
  immediately.
- Any direct SQLite read from the Next.js side. The app must go
  through the Rust API — if you reach for `better-sqlite3` from a
  Next route handler, stop and add an endpoint to the Rust API
  instead.
- Any direct JSONL read from Next. Same rule.
