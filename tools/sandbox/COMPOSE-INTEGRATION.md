# sentinel-viz × sentinel-sandbox compose integration

The single-file compose stack at `tools/sandbox/docker-compose.yml`
brings up the sentinel observability mesh in one shot:

- `sentinel-bridge` — tails hook-invocation JSONLs into the
  activegraph SQLite store
- `viz-api` — Rust HTTP API on port 8082 (host-forwarded)
- `viz-next` — Next.js dashboard on port 8083 (**browse from your
  host at <http://localhost:8083>**)
- `sandbox-dev` — long-running dev container with Claude Code CLI,
  wired to the bridge via a metrics bind-mount and the canonical
  hooked Claude settings profile. Also forwards the agent-workload
  port range `18000-18099` so the in-container Claude can spin up
  ad-hoc preview services the host can browse.

> The stack is intentionally minimal. The memory-server / qdrant /
> neo4j trio that earlier drafts of this compose carried has been
> dropped — that path isn't load-bearing for sentinel's current
> observability flow, and three services worth of pull/boot time
> for zero day-1 benefit doesn't pay rent. When memory-server lands
> in production wiring, add it back under `--profile memory`.

Bring it up:

```
bash tools/sandbox/sandbox-up.sh
# wait ~30s for viz-next's first build, then:
open http://localhost:8083
```

`sandbox-up.sh` is a thin wrapper around `docker compose ... up -d`
that adds host-UID propagation and pre-flight bind-mount checks.
You can also invoke compose directly:

```
docker compose -f tools/sandbox/docker-compose.yml up -d
```

Exec into the dev container:

```
docker compose -f tools/sandbox/docker-compose.yml exec sandbox-dev bash
```

Tear it down:

```
bash tools/sandbox/sandbox-down.sh           # stop + remove containers, keep volumes
bash tools/sandbox/sandbox-down.sh --purge   # also drop volumes (destructive)
```

## Important: only ONE bridge can run

The `sentinel-bridge` service writes the activegraph SQLite, and
`viz-api` reads it. If your host already has a `sentinel-bridge tail`
process running against the same metrics dir, **stop it before `up`**.
Two bridges tailing the same metrics would race on the SQLite writes
and corrupt the store. The container's bridge is the new authority;
the host bridge becomes redundant.

## The agent workload port range

`sandbox-dev` forwards `127.0.0.1:18000-18099` host→container. The
in-container claude can bind any port in that range on `0.0.0.0` and
the operator can browse the result from the host at
`http://localhost:<port>`. Convention:

- Pick a free port from `$SENTINEL_SANDBOX_PORT_RANGE` (set by the
  compose to `18000-18099`).
- Bind your service on `0.0.0.0:<port>` (not `127.0.0.1:<port>` —
  loopback inside the container isn't reachable from the host).
- Surface the URL: "running at <http://localhost:18002>".

The range is sized at 100 ports for headroom; pick anything in it.

## Browser ↔ viz-api routing

The viz-next image bakes `NEXT_PUBLIC_VIZ_API` at build time. The
default is `http://localhost:8082`, which works because the compose
forwards `viz-api:8082` to `127.0.0.1:8082` on the host. If your host
already has something on port 8082, override at build time:

```
VIZ_API_PUBLIC_URL=http://localhost:8092 \
  docker compose -f tools/sandbox/docker-compose.yml build viz-next
```

and forward 8092 instead (edit the compose).

## What the sandbox-dev service does at boot

The compose mounts `sandbox-bootstrap.sh` into the container and
invokes it as the entrypoint command. The bootstrap is idempotent —
first boot costs ~3 minutes for the cargo install; cached boots
return in <1s. It provisions:

1. **sentinel binary** — `cargo install` from the mounted repo into
   a state-volume-backed cargo-bin (so it survives container
   restarts without rebuild).
2. **PATH wiring** — `/usr/local/bin` when running as root, else
   `~/.bashrc` + `/etc/profile.d/sentinel-sandbox.sh`.
3. **gh CLI** — apt-installed via the official signed keyring +
   `cli.github.com` repo. Required for `gh pr …` workflows and to
   satisfy host-inherited credential helpers.
4. **Container-local gitconfig** — host `~/.gitconfig` is
   ro-mounted; the bootstrap writes a fresh writable gitconfig to
   `$STATE_DIR/gitconfig` and points git at it via
   `GIT_CONFIG_GLOBAL` (wired into `~/.bashrc` +
   `/etc/profile.d/`). The generated file inherits
   `user.name`/`user.email` from the ro host config and adds an
   HTTPS→SSH URL rewrite for github.com (SSH is the only working
   auth path — host `~/.ssh` is also ro-mounted with the key in
   place).
5. **Worktree pointer repair** — host-created worktrees under
   `.worktrees/` and `.claude/worktrees/` have `.git` files
   pointing at `/home/<host-user>/...` paths that don't resolve
   inside the container. The bootstrap rewrites pointers to
   `/workspace/sentinel/...` and, where the central `.git` is
   missing the worktree registration entirely, constructs the
   registration manually so all worktrees become usable.

## How the bridge wiring works

- `sentinel hook` resolves its JSONL output path via
  `crates/sentinel-application/src/paths.rs::claude_dir`, which
  honors `SENTINEL_CLAUDE_DIR` first. The compose stack sets
  `SENTINEL_CLAUDE_DIR=/workspace/.container-state/claude`, so the
  metrics directory ends up at
  `/workspace/.container-state/claude/sentinel/metrics/`.
- The bind mount in `docker-compose.yml` makes that exact path the
  host's `~/.claude/sentinel/metrics/`, which is in the bridge's
  `METRICS_DIRS` list (`tools/sentinel-bridge/src/ingest.rs`).
- The bridge's incremental tail (byte-offset state at
  `~/.agents/scratch/activegraph-bridge/bridge.state.json`) picks
  up new lines as they're written. ~250ms latency from hook fire
  to bridge ingest.
- The Rust viz-api emits the snapshot via SSE
  (`/api/stream?include_hooks=true`).
- The Next.js dashboard renders the new session with its harness
  chip (`claude`) and tool-categorised sparklines.

## Verified end-to-end

Reproduced 2026-05-27 against the live `sentinel-sandbox-dev`
container:

1. Built `sentinel` inside the container with the bootstrap's
   `cargo install` path.
2. Fired a synthetic hook
   (`echo '{...}' | sentinel hook --event PreToolUse --standalone`).
   The binary wrote multi-hook fanout records to
   `/workspace/.container-state/claude/sentinel/metrics/hook-invocations.jsonl`.
3. Via the metrics bind-mount, those records landed at the host's
   `~/.claude/sentinel/metrics/hook-invocations.jsonl`.
4. The host's `sentinel-bridge` ingested
   `SentinelHookInvocation` objects within 5s, and the new
   synthetic session appeared at `/api/graph?include_hooks=true`
   tagged `harness=claude`.

## Opt-out

`SENTINEL_HOOKS=off` should swap the settings mount to
`tools/sandbox/claude-settings-hookless.json` (file pending —
the original sandbox-tools branch has a stub that needs to land
on a future commit). Until then, the hooked profile is the only
shipped variant.

## Reaching the viz dashboard from inside the container

The compose's `extra_hosts: host.docker.internal:host-gateway`
entry lets the container reach the host's bridge / viz-api at
`http://host.docker.internal:8082`. The Next.js dashboard
(port 3000) isn't currently meant to be browsed from inside the
container; if you want in-sandbox dashboard access, expose port
3000 host-side and point `NEXT_PUBLIC_VIZ_API` at
`http://host.docker.internal:8082`.
