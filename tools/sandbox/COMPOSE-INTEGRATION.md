# sentinel-viz × sentinel-sandbox compose integration

Wire sentinel-viz (the host's bridge + dashboard) into the
`infra/sandbox/` compose stack so the in-container Claude session's
hook events flow up to the host bridge and render in the dashboard.

This is the compose-level counterpart to PR #3's grind-container
hook wiring. The grind path uses `~/.agents/notes/sentinel-docker-dev/`
scripts; this one wires the long-running `sentinel-sandbox-dev`
container brought up by `infra/sandbox/docker-compose.yml`.

## Verified end-to-end inside the running sandbox

Reproduced 2026-05-27 against the live `sentinel-sandbox-dev`
container (the `feat-memory-server-vertical-slice` worktree's
compose stack):

1. Copied `tools/sandbox/claude-settings-hooked.json` into
   `/workspace/.container-state/claude/settings.json`. Claude reads
   it via `$HOME/.claude/settings.json` (which is a symlink to that
   path inside the named-volume layout).
2. Built `sentinel` inside the container with
   `cargo install --path /workspace/sentinel/crates/sentinel-cli
   --root /workspace/.container-state/cargo-bin`. Required pinning
   the sibling `legatus-consul-agent` repo to commit `f0a07bb` —
   `89e6862` introduced an `operator_id` field on `RegisterSession`
   that breaks the current `sentinel-legatus` lib.
3. Fired a synthetic hook (`echo '{...}' | sentinel hook --event
   PreToolUse --standalone`). The binary wrote 9 hook-invocation
   records (multi-hook fanout) to
   `/workspace/.container-state/claude/sentinel/metrics/hook-invocations.jsonl`.
4. Manually appended that file's contents into the host's
   `~/.claude/sentinel/metrics/hook-invocations.jsonl` (simulating
   the bind mount described below). The host's `sentinel-bridge`
   daemon ingested 13 SentinelHookInvocation objects within 4s, and
   the new `sandbox-test-001` SentinelSession appeared at
   `/api/graph?include_hooks=true` tagged `harness=claude`.

The plumbing works end-to-end; the only missing piece is the
compose-level mount that makes step 4 automatic.

## Required compose-file additions

Apply to `infra/sandbox/docker-compose.yml` in the
`feat-memory-server-vertical-slice` worktree (or whatever branch
merges sentinel-viz with the sandbox stack):

```yaml
services:
  sandbox-dev:
    # ... existing config ...
    extra_hosts:
      # Lets the in-container claude / curl reach the host's
      # sentinel-viz-api at host.docker.internal:8082. Linux needs
      # the host-gateway alias explicit.
      - "host.docker.internal:host-gateway"
    volumes:
      # ... existing volumes ...

      # Sentinel hook output crosses up to the host bridge.
      # Container writes to $SENTINEL_CLAUDE_DIR/sentinel/metrics/;
      # this bind makes that path the SAME directory the host bridge
      # already tails (~/.claude/sentinel/metrics/).
      - ${HOME}/.claude/sentinel/metrics:/workspace/.container-state/claude/sentinel/metrics

      # The hooked settings.json so claude inside the container fires
      # `sentinel hook --event …`. Sourced from this repo's
      # tools/sandbox/ so the file is versioned with the bridge.
      - ${SENTINEL_REPO:-../..}/tools/sandbox/claude-settings-hooked.json:/workspace/.container-state/claude/settings.json:ro

      # Sibling consul agent repo — sentinel-legatus path-depends on
      # crates here. Pin to f0a07bb (last commit before the breaking
      # `RegisterSession.operator_id` change in 89e6862) or update
      # sentinel-legatus to match the new shape.
      - ${HOME}/firefly/legatus-consul-agent:/workspace/legatus-consul-agent:rw
```

And `Dockerfile.dev` should pre-build the sentinel binary so the
container is hook-ready out of the box (avoids the ~3min cargo
install on first claude run):

```dockerfile
# Pre-build sentinel CLI so 'sentinel hook' is on PATH from the
# first container start. Cached in the image; the named cargo
# volume only re-uses build artifacts for in-tree work.
COPY --from=consul-builder /tmp/legatus-consul-agent /workspace/legatus-consul-agent
RUN cd /workspace/sentinel \
 && cargo install --path crates/sentinel-cli --root /usr/local --locked \
 && rm -rf /workspace/sentinel/target
```

Or, less invasive: add a `cargo install` step to the container's
entrypoint shell-rc so the first interactive shell triggers the
build (matches the run-plan.sh pattern in
`~/.agents/notes/sentinel-docker-dev/`).

## Opt-out

`SENTINEL_HOOKS=off` should swap the settings mount to
`tools/sandbox/claude-settings-hookless.json` (see the existing
opt-out flow in PR #3's launcher patches).

## Why this works

- `sentinel hook` resolves its JSONL output path via
  `crates/sentinel-application/src/paths.rs::claude_dir`, which
  honors `SENTINEL_CLAUDE_DIR` first. The compose stack already
  sets `SENTINEL_CLAUDE_DIR=/workspace/.container-state/claude`,
  so the metrics dir ends up at
  `/workspace/.container-state/claude/sentinel/metrics/`.
- The bind mount above makes that exact path also the host's
  `~/.claude/sentinel/metrics/`, which is in the bridge's
  `METRICS_DIRS` list (`tools/sentinel-bridge/src/ingest.rs`).
- The bridge's incremental tail (byte-offset state at
  `~/.agents/scratch/activegraph-bridge/bridge.state.json`) picks
  up new lines as they're written. ~250ms latency from hook fire
  to bridge ingest.
- The Rust viz-api emits the snapshot via SSE
  (`/api/stream?include_hooks=true` — see PR commit 4fa56c4).
- The Next.js dashboard renders the new session with its harness
  chip (`claude`) and tool-categorised sparklines.

## Outstanding follow-up

- `legatus-consul-agent` pin needs to be either committed (a
  Cargo.lock entry, or a `.gitmodules`-style submodule) or the
  `sentinel-legatus` lib updated to match the post-89e6862 API.
- The `extra_hosts` line lets the container reach the host's
  bridge / viz-api, but the **dashboard** (Next.js at port 3000)
  isn't currently meant to be browsed from inside the container.
  If the operator wants in-sandbox dashboard access, also expose
  port 3000 host-side and update `NEXT_PUBLIC_VIZ_API` to
  `http://host.docker.internal:8082` for that case.
