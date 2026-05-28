# How sentinel sandboxing works

The sandbox is a docker stack that gives operators a clean
container to run claude / codex / autonomous grinds in while
sentinel's observability layer captures every hook event the
session fires. The host stays clean; the container's work is
fully visible in the dashboard.

This doc explains the **conceptual model** — what's isolated,
what crosses the boundary, why, and where the model breaks.
For the wiring details see `COMPOSE-INTEGRATION.md`; for the
settings/launcher knobs see `README.md`.

## The two layers

```
  ┌─── host machine ────────────────────────────────────────────┐
  │                                                             │
  │   ~/.claude/                          ~/.claude/sentinel/   │
  │   ├── settings.json    (host claude)  └── metrics/          │
  │   ├── .credentials.json (RO mount)        └── *.jsonl       │
  │   └── projects/        (untouched)           ▲              │
  │                                              │              │
  │   ┌── sandbox-dev container ────────────┐    │              │
  │   │                                     │    │              │
  │   │  /workspace/.container-state/       │    │              │
  │   │  └── claude/        (container's    │    │              │
  │   │      ├── settings.json   private    │    │              │
  │   │      ├── projects/      space —     │    │              │
  │   │      └── sentinel/      separate    │    │              │
  │   │          └── metrics/   from host)  │    │              │
  │   │              └── *.jsonl   ─bind────┘    │              │
  │   │                            same inode    │              │
  │   │                                          ▼              │
  │   │  (claude in here fires hooks            ─── ingest ───  │
  │   │   → sentinel hook writes JSONL                          │
  │   │   → bridge tails JSONL → SQLite)        sentinel-bridge │
  │   │                                                         │
  │   └─────────────────────────────────────┘                   │
  │                                                             │
  │   viz-api (port 8082)  ← reads SQLite ←  bridge SQLite      │
  │   viz-next (port 8083) ← reads viz-api                      │
  │                                                             │
  └─────────────────────────────────────────────────────────────┘
```

## What's isolated

Inside the sandbox-dev container:

- **Claude settings + project history.** `/home/dev/.claude` is a
  symlink to the container-only state volume
  (`/workspace/.container-state/claude/`). The container claude
  has its own settings.json, its own project memory, its own
  chat history. None of it touches the host's `~/.claude/`.
- **Filesystem writes.** Everything outside the bind-mounts goes
  to the container's overlay; gone on container removal.
- **Network identity.** The container has its own network
  namespace. Outbound is allowed (for cargo / pnpm / Claude API
  / OpenRouter / etc.); inbound is restricted to the
  host-published port mappings — viz-api/-next + the
  agent-workload port range (18000-18099).
- **Process tree.** No process inside the container can see or
  signal a host process. Claude in the container can't read your
  host shell history, attach to your host tmux sessions, or
  signal your host editor.

## What crosses the boundary (and why)

| Bind-mount (host → container) | Mode | Why |
|---|---|---|
| `~/.claude/.credentials.json` | **RO** | OAuth token for the Claude Max subscription. RO so an in-container compromise can't rewrite it. |
| `~/.claude/sentinel/metrics/` | **RW** | Hook output. The container's sentinel writes JSONL here; the bridge tails the same inode. This is the ONLY RW host path the container has. |
| `~/firefly/legatus-consul-agent` | **RW** | Sister repo. `sentinel-legatus` has a path-dep on its crates. RW so the container can edit it the same way it edits sentinel. |
| `${SENTINEL_REPO}` (the sentinel checkout) | **RW** | The repo under work. RW because the container is literally a dev environment for sentinel. |
| `tools/sandbox/claude-settings-hooked.json` | **RO** | Canonical hooks profile. Pinned in the repo so the file travels with the code. |
| `tools/sandbox/sandbox-bootstrap.sh` | **RO** | Container provisioning script. RO because it's the entrypoint. |

The boundary is intentionally asymmetric: the host can read the
container's work output (via the metrics dir + the SQLite store
in `bridge-state`), but the container can write to **only one**
host path — the metrics directory.

## Why hooks fire from the container

`sentinel hook` resolves its JSONL output path through
`crates/sentinel-application/src/paths.rs::claude_dir`, which
honors `SENTINEL_CLAUDE_DIR` first. The compose sets that to
`/workspace/.container-state/claude`, so the metrics dir lands
at `/workspace/.container-state/claude/sentinel/metrics/`.

The bind-mount makes that exact path the host's
`~/.claude/sentinel/metrics/`. The `sentinel-bridge` service —
which has its own bind-mount of the same host directory —
tails the JSONL and ingests new lines into the activegraph
SQLite store. `viz-api` reads that store. The dashboard reads
viz-api. End-to-end latency: ~250ms from hook fire to dashboard.

## Where the model breaks (don't do these)

- **Running another bridge.** Two `sentinel-bridge` processes
  tailing the same metrics directory will race on SQLite writes
  and corrupt the store. The compose stack's bridge is the sole
  authority; stop any host-side bridge before bringing the stack
  up.
- **Sharing the cargo/pnpm caches with the host.** The
  `sandbox-cache` named volume is intentionally container-only.
  Sharing your host's `~/.cargo` would leak host build artifacts
  into the container and vice versa — defeats the
  reproducibility goal of a sandbox.
- **Bind-mounting your full host `~/.claude/` into the container.**
  The metrics dir is the deliberately narrow seam. Mounting the
  whole `.claude/` would let the container read your host
  project history, settings, and (with write mode) overwrite
  them.
- **Binding container services on `127.0.0.1`.** Loopback inside
  the container is unreachable from the host. Bind on
  `0.0.0.0:<port>` instead. The compose's agent-workload port
  range (18000-18099) and the conventions in
  `COMPOSE-INTEGRATION.md` follow this rule.

## The fleet escape hatch

The default stack runs ONE sandbox-dev container. For
benchmarking / regression-search / A-B comparisons, see
`sandbox-fleet.sh` — it spins N parallel sandbox-dev containers
that share the bridge but have isolated cache + state volumes.
Each replica gets its own slice of the agent-workload port
range (`FLEET_PORT_BASE` + `FLEET_PORT_SLICE`) so they don't
collide.

The fleet pattern is intentionally outside the daily compose
flow — `sandbox-up.sh` brings up the regular stack, and
`sandbox-fleet.sh up --count N --plan ...` opts in to the
multi-replica run on top of it. The bridge handles N
concurrent writers fine because each session is identified by
its own `session_id`.

## Day-1 quick reference

```bash
# Bring up the stack
bash tools/sandbox/sandbox-up.sh

# Verify it works end-to-end
bash tools/sandbox/sandbox-smoke-test.sh

# Open the dashboard
xdg-open http://localhost:8083  # or just visit it in a browser

# Shell into the dev container
docker compose -f tools/sandbox/docker-compose.yml exec sandbox-dev bash

# Run an autonomous grind on a plan file
bash tools/sandbox/sandbox-grind.sh /abs/path/to/plan.md

# Spin a 4-replica fleet against the same plan
bash tools/sandbox/sandbox-fleet.sh up --count 4 --plan /abs/path/to/plan.md

# Tear down (keep volumes — hot start next time)
bash tools/sandbox/sandbox-down.sh

# Tear down + wipe state (full clean slate, ~3min rebuild next up)
bash tools/sandbox/sandbox-down.sh --purge
```
