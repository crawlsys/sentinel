# How sentinel sandboxing works

The sandbox is one docker container — `sentinel-sandbox-dev` —
that gives an operator a clean Linux environment to run sentinel
work in: interactive claude sessions, autonomous grinds, fleet
benchmarks. The host stays untouched; the container's hook
output crosses a single narrow seam back to the host's metrics
directory.

This doc covers the **conceptual model**: what's isolated, what
crosses the boundary, why, and where the model breaks. For
day-1 usage see `README.md`.

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
  │   │                                                         │
  │   └─────────────────────────────────────┘                   │
  │                                                             │
  │   (whatever the operator already runs to consume            │
  │    ~/.claude/sentinel/metrics/ continues consuming it —     │
  │    this PR makes no assumption about that surface.)         │
  │                                                             │
  └─────────────────────────────────────────────────────────────┘
```

## What's isolated

Inside `sentinel-sandbox-dev`:

- **Claude settings + project history.** `/home/dev/.claude` is
  a symlink to the container-only state volume
  (`/workspace/.container-state/claude/`). The container claude
  has its own settings.json, its own project memory, its own
  chat history. None of it touches the host's `~/.claude/`.
- **Filesystem writes.** Everything outside the bind-mounts goes
  to the container's overlay; gone on container removal.
- **Network identity.** The container has its own network
  namespace. Outbound is allowed (cargo, pnpm, Claude API, etc.);
  inbound is restricted to the host-published port mappings (the
  agent-workload port range).
- **Process tree.** No process inside the container can see or
  signal a host process.

## What crosses the boundary (and why)

| Bind-mount (host → container) | Mode | Why |
|---|---|---|
| `~/.claude/.credentials.json` | **RO** | OAuth token for the Claude Max subscription. RO prevents *tampering* (the container can't rewrite or revoke it), but **does not make it secret**: the bearer token is plaintext-readable to every process in the container. For a bearer credential, read — not write — is the real threat. See the read-vs-write caveat below. |
| `~/.claude/sentinel/metrics/` | **RW** | Hook output. The container's sentinel writes JSONL here; the operator's host-side consumer tails the same inode. This is the only RW host path *outside the working repos* — the container's sentinel writes observability events here, but cannot reach the rest of the host's `~/.claude/`. |
| `~/firefly/legatus-consul-agent` | **RW** | Sister repo. `sentinel-legatus` has a path-dep on its crates. RW so the container can edit it the same way it edits sentinel. |
| `${SENTINEL_REPO}` (the sentinel checkout) | **RW** | The repo under work. RW because the container is literally a dev environment for sentinel. |
| `tools/sandbox/claude-settings-hooked.json` | **RO** | Canonical hooks profile. Pinned in the repo so the file travels with the code. |
| `tools/sandbox/sandbox-bootstrap.sh` | **RO** | Container provisioning script. RO because it's the entrypoint. |

The boundary is intentionally asymmetric. The container has
exactly three RW host paths: the metrics directory and the two
working repos it is a dev environment for (`${SENTINEL_REPO}` and
`legatus-consul-agent`). It has **no** write access to the rest of
the host's `~/.claude/` — settings, project history, and the
credentials file are either unmounted or RO. So a compromised
in-container process can edit the source trees under work and
deposit observability events, but it cannot rewrite the host's
Claude config/state or its OAuth token.

**Read-vs-write caveat (credentials).** The asymmetry above is
about *write* access. It does **not** protect the *confidentiality*
of the secrets mounted RO. The `.credentials.json` OAuth token —
and the `~/.ssh` private key the bootstrap relies on — are
plaintext-readable inside the container, and outbound network is
open. So a compromised or prompt-injected grind can exfiltrate the
operator's personal Max token (and SSH key) even though it can't
overwrite them. Treat a sandbox compromise as a credential
compromise: only run trusted plans, and rotate the token/key if a
sandbox is ever exposed to untrusted input. Prefer pointing the RO
mount at a dedicated/throwaway credential rather than your personal
`~/.claude/.credentials.json` — set **`SENTINEL_CRED_FILE`** to the
throwaway token's path before launching (honored by `sandbox-up.sh`,
the base compose, and `sandbox-fleet.sh`). This matters most under
`sandbox-fleet.sh`, which mounts the *same* token into every
replica, so a single compromised replica exposes one shared
credential across the whole fleet.

## The scale model — sandbox-fleet.sh

The default compose runs ONE container. `sandbox-fleet.sh`
opts in to multiple:

```
sandbox-fleet.sh up --count 4 --plan /abs/path/to/plan.md
```

What this gets you, by intended use:

- **Regression search across sentinel commits.** Set
  `SENTINEL_REPO` per replica to a different worktree. Each
  replica runs the same plan against a different sentinel
  build, side-by-side. Useful for "which commit broke X" hunts.
- **Hook on/off A/B.** Half the fleet runs with
  `claude-settings-hooked.json`, half with `-hookless.json`.
  Plan throughput / outcome divergence quantifies the hook
  layer's impact.
- **Skill / config variants.** Per-replica env vars (e.g.
  `SENTINEL_LLM_PREFER`) drive different routing behavior; the
  plan completion logs surface which variant won.
- **Throughput soak.** 16 fleet replicas firing real plans
  in parallel surfaces contention bugs in `sentinel hook` writes,
  the metrics directory, host-side ingest paths, etc.

### Replica isolation

Each fleet replica gets its own `sandbox-cache-<i>` and
`sandbox-state-<i>` named volumes. **Caches do NOT cross
replicas** — first boot is ~3min × N for the cargo build. This
is deliberate: a shared cache would let one replica's cargo
state poison another's build determinism. For benchmarking the
cost matters less than the validity.

### Replica port slicing

The compose's `sandbox-dev` reserves agent-workload ports
18000-18099 for itself. Fleet replicas use a separate base
(`FLEET_PORT_BASE`, default 19000) carved into per-replica
slices (`FLEET_PORT_SLICE`, default 10 ports each):

```
replica 1 → 19000-19009
replica 2 → 19010-19019
replica 3 → 19020-19029
…
```

So a 16-replica fleet uses 19000-19159. Override the base /
slice via env if you need more headroom or a different range.

## Where the model breaks (don't do these)

- **Sharing the cargo/pnpm caches with the host.** The
  `sandbox-cache` named volume is intentionally container-only.
  Sharing your host's `~/.cargo` would leak host build artifacts
  into the container and vice versa — defeats reproducibility.
- **Bind-mounting your full host `~/.claude/` into the container.**
  The metrics dir is the deliberately narrow seam. Mounting the
  whole `.claude/` would let the container read your host
  project history, settings, and (with write mode) overwrite
  them.
- **Binding container services on `127.0.0.1`.** Loopback inside
  the container is unreachable from the host. Bind on
  `0.0.0.0:<port>` instead. The agent-workload port range
  conventions follow this rule.
- **Running fleet at high N on a small host.** Each replica is
  a full Rust+Node+Claude stack. RAM usage scales linearly with
  N. 4 replicas is comfortable on a 16GB host; 16 wants
  ≥64GB and fast disk. Smoke-test with `--count 2` before
  scaling.
