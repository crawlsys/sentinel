# sentinel-viz × sentinel-sandbox compose integration

The single-file compose stack at `tools/sandbox/docker-compose.yml`
brings up sentinel-viz × sentinel-sandbox in one shot:

- `qdrant` + `neo4j` + `memory-server` — sentinel's memory stores
- `sandbox-dev` — long-running dev container with Claude Code CLI,
  wired to the host sentinel-viz bridge via a metrics bind-mount
  and the canonical hooked Claude settings profile

Bring it up:

```
docker compose -f tools/sandbox/docker-compose.yml up -d
docker compose -f tools/sandbox/docker-compose.yml exec sandbox-dev bash
```

Tear it down:

```
docker compose -f tools/sandbox/docker-compose.yml down
```

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
