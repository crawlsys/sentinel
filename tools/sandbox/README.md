# sentinel sandbox — docker containers for safe sentinel work at scale

This directory ships a docker-compose stack + a small set of
launch scripts that spin up `sentinel-sandbox-dev` containers:
isolated Linux environments with Claude Code, sentinel, and the
toolchain pre-baked, so an operator can run sentinel work
(interactive sessions, autonomous grinds, fleet benchmarks)
without touching the host's `~/.claude/` or polluting the host
filesystem.

This PR is **only about sandboxing**. Observability (the
sentinel-bridge, viz-api, viz-next dashboard) lives outside this
scope — the sandbox writes hook JSONL to a well-known host path
and whatever the operator already runs to consume that path
keeps consuming it.

## Day 1

```bash
# Build + start the sandbox container (~3 min first time)
bash tools/sandbox/sandbox-up.sh

# Verify the wiring (4 checks, ~5 seconds)
bash tools/sandbox/sandbox-smoke-test.sh

# Exec into it
docker compose -f tools/sandbox/docker-compose.yml exec sandbox-dev bash

# Or run an autonomous claude session against a plan
bash tools/sandbox/sandbox-grind.sh /abs/path/to/plan.md

# Tear down (keeps volumes — hot start next time)
bash tools/sandbox/sandbox-down.sh

# Tear down + wipe all sandbox state (~3min rebuild next up)
bash tools/sandbox/sandbox-down.sh --purge
```

## Scripts

| Script | Purpose |
|---|---|
| `sandbox-up.sh` | Build (if needed) + start the sandbox-dev container. Pre-flights required host paths. |
| `sandbox-down.sh` | Stop the container. `--purge` also drops volumes. |
| `sandbox-smoke-test.sh` | 4-check verify: container running, binaries on PATH, metrics bind-mount round-trips host↔container, agent-workload port forwarding. |
| `sandbox-grind.sh` | One-shot autonomous claude run inside the sandbox against a plan file. `--detach` for background. |
| `sandbox-fleet.sh` | Spin N parallel sandbox containers with isolated cache+state. Subcommands: `up / down / logs / status`. The scale surface — see SANDBOX.md for use cases. |
| `sandbox-bootstrap.sh` | In-container entrypoint that builds sentinel, installs gh, sets up a container-local gitconfig, repairs worktree pointers. Idempotent. |

## What the container provides

- **Rust 1.x toolchain** for building sentinel from source.
- **Node 22 + pnpm** for any JS-side tooling.
- **Claude Code CLI** installed globally — inherits the host's
  Claude Max OAuth token via a RO bind of `.credentials.json`.
  RO blocks tampering, not reads: the token is plaintext-readable
  in the container, so treat a sandbox compromise as a token
  compromise. Set `SENTINEL_CRED_FILE` to a dedicated/throwaway
  token for untrusted plans or fleet runs. See the read-vs-write
  caveat in `SANDBOX.md`.
- **gh CLI** for any `gh pr …` workflows.
- **Container-local `$HOME/.claude`** (symlinked into a named
  volume) so claude state doesn't leak into the host.
- **`sentinel hook` PATH wiring** so the canonical hooked
  settings profile (claude-settings-hooked.json) fires sentinel
  hooks on every tool call.
- **Forwarded agent-workload ports 18000-18099** so claude in
  the container can spin up ad-hoc preview servers the host can
  browse.

## Hook output reaches the host

The container's `$SENTINEL_CLAUDE_DIR/sentinel/metrics/` is
bind-mounted to the host's `~/.claude/sentinel/metrics/` — same
inode. The container's sentinel writes JSONL there; whatever
process on the host already tails that directory (typically a
host-side `sentinel-bridge`) picks it up. This PR makes no
assumptions about what consumes the directory; it only
guarantees the write surfaces.

## Settings profiles

| File | Use |
|---|---|
| `claude-settings-hooked.json` | Default. Fires `sentinel hook --event <Name>` on every PreToolUse / PostToolUse / etc. |
| `claude-settings-hookless.json` | `SENTINEL_HOOKS=off`. Empty hook map — claude runs without sentinel observability. Useful for hook-on / hook-off A/B fleet runs. |

Toggle by editing the compose's `claude-settings-*.json`
bind-mount line, or set `SENTINEL_HOOKS=off` and re-launch via a
launcher that picks the corresponding file.

## Env knobs

| Var | Default | Effect |
|---|---|---|
| `SENTINEL_REPO` | `../..` | Host path to the sentinel checkout to mount RW. |
| `SENTINEL_CRED_FILE` | `~/.claude/.credentials.json` | Host path to the Claude token mounted RO into the container. Point at a dedicated/throwaway credential for untrusted plans or fleet runs (the token is plaintext-readable in-container — see the read-vs-write caveat in `SANDBOX.md`). |
| `HOST_UID` / `HOST_GID` | host's `id -u/-g` | UID/GID baked into the image so bind-mounted files stay writable. |
| `SENTINEL_CLAUDE_DIR` | `/workspace/.container-state/claude` | Where the container's sentinel writes JSONL (set by compose). |
| `SENTINEL_SANDBOX_PORT_RANGE` | `18000-18099` | Range claude-in-container is expected to bind preview services on. |
| `FLEET_PORT_BASE` / `FLEET_PORT_SLICE` | `19000` / `10` | First port + per-replica slice width for `sandbox-fleet.sh`. |

## See also

- `SANDBOX.md` — isolation contract + scale model (read this
  before running `sandbox-fleet.sh` against anything serious).
- `sandbox-bootstrap.sh` header — what bootstrap actually does
  in-container, in detail.
