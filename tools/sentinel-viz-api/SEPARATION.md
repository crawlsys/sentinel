# sentinel-viz-api — peel-off contract

> **WORKSTREAM: sentinel-viz** — this crate is intended to be
> extractable from the Sentinel monorepo into its own repository
> (`sentinel-viz`) without rewriting any of its code. This document
> is the contract that has to hold for that extraction to be
> mechanical.

Per user directive (2026-05-25):
> "ensure that work can easily be peeled off into a separate
> workstream when needed (via comments, graph, smoke signals, idgaf)
> — im pretty sure a lot of its already intermingled — deal with that
> too (and tag it)"

## What's separable today

This crate has **no** direct compile-time dependency on any other
Sentinel crate. The standalone `[workspace]` table in its
`Cargo.toml` exists precisely to keep the broken-root workspace from
pulling it into the Sentinel build graph.

Runtime contracts that DO span the boundary:

| Contract | Shape | Owner today | After peel-off |
|---|---|---|---|
| SQLite event store | path `~/.agents/scratch/activegraph-bridge/sentinel.db`, schema `events(seq, id, type, actor, payload, frame_id, caused_by, timestamp, run_id)` plus `runs`, `meta` | Sentinel's `sentinel_bridge.py` (in `tools/sentinel-viz/`) writes | Same; viz READS only |
| Transcript JSONL | `~/.claude/projects/*/<sid>.jsonl` + `~/.claude-sentinel/projects/*/<sid>.jsonl` | Claude Code writes | Same; viz reads |
| Container bind path | `/workspace/sentinel/tools/sentinel-viz-api` under the Docker dev image | `legatus-docker-dev.sh` mounts the host repo | Mount the standalone repo instead |

## Tag pattern

Every cross-boundary touchpoint in this crate is annotated with a
single-line comment of the form `// WORKSTREAM: <name> — <note>`.
Grep `WORKSTREAM:` to find them all. Today they are:

```
WORKSTREAM: sentinel-bridge — reads `events` table the bridge owns
WORKSTREAM: claude-code — reads transcript JSONLs Claude Code owns
WORKSTREAM: sentinel-viz — internal to this crate
```

If you add a new cross-boundary call, add the tag. CI grep doesn't
yet enforce this; for now it's a manual hygiene rule.

## How to extract

The day you decide to split:

1. `git filter-repo --path tools/sentinel-viz-api --path tools/sentinel-viz-next` against a clone of the Sentinel repo.
2. Move the filtered tree to a new repo (`sentinel-viz`).
3. Drop the `[workspace]` table — the standalone crate now lives at
   the repo root.
4. In the new repo, document the SQLite + JSONL contracts as part of
   the public README rather than this private SEPARATION.md.
5. Bump `sentinel_bridge.py` over to the new repo too if you want a
   single home for the data path; otherwise leave it where it is and
   document the link.

## Smoke signals if the boundary breaks

- Anything in `src/` reaching outside `tools/sentinel-viz-api/` for a
  Rust import. The standalone `[workspace]` table will refuse to
  resolve it. Treat the compile error as the contract enforcing
  itself.
- Anything writing to the SQLite store from this crate. The crate
  opens with `OPEN_READ_ONLY` — any new write call has to add a
  separate connection. If you find yourself doing that, stop and
  ask whether the write belongs in the bridge instead.
