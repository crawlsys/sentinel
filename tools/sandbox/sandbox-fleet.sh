#!/usr/bin/env bash
# sandbox-fleet.sh — spin N parallel sandbox-dev containers
# against a plan file. The "I hate money" surface: each replica
# is a fully-isolated container with its own cache + state
# volumes, so you can throw 4-16 of them at the same plan and
# benchmark variance / regression-search / model A-B comparisons
# without containers stepping on each other.
#
# What it's good for:
#   - Benchmarking: same plan against multiple sentinel commits
#     (set SENTINEL_REPO per replica to a different worktree).
#   - Skill / hook A/B: half the fleet runs with hooks on, half
#     with hooks off (claude-settings-hookless.json).
#   - Throughput test: 16 sandboxes running real plans
#     simultaneously to surface contention bugs in
#     `sentinel hook`, the metrics dir, the host bridge ingest
#     path, etc.
#
# What it's NOT good for:
#   - Cheap exploratory work — each replica has its own
#     sandbox-cache volume, so first boot is ~3min × N.
#   - Anything sensitive to host port collisions — the agent
#     workload port range gets carved into N stripes so each
#     replica has its own slice.
#
# Usage:
#   bash tools/sandbox/sandbox-fleet.sh up   --count 4 --plan /abs/plan.md
#   bash tools/sandbox/sandbox-fleet.sh logs --replica 2
#   bash tools/sandbox/sandbox-fleet.sh down
#
# Each replica is named sentinel-sandbox-fleet-<i>; all of them
# share the host's ~/.claude/sentinel/metrics/ as their hook
# output sink (and the operator's existing host-side
# sentinel-bridge ingests from there).
#
# This is intentionally a thin orchestrator over `docker run`,
# not a compose extension — fleet runs are short-lived and the
# overhead of per-replica compose files outweighs the win.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

FLEET_PREFIX="${FLEET_PREFIX:-sentinel-sandbox-fleet}"
LOG_ROOT="${FLEET_LOG_DIR:-/tmp/sentinel-sandbox/fleet}"
# Each replica gets a 10-port slice of the agent workload range
# starting at this base. Replica i binds ports BASE+(i*SLICE)
# .. BASE+(i*SLICE)+(SLICE-1).
PORT_BASE=${FLEET_PORT_BASE:-19000}
PORT_SLICE=${FLEET_PORT_SLICE:-10}

say() { printf '\033[1;36m[fleet]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[fleet][ERR]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<EOF
usage:
  $0 up --count N --plan /abs/plan.md [--detach]
        Spin up N replicas + run the grind on each.
  $0 down
        Stop and remove all fleet replicas. The non-fleet
        sandbox-dev container (if any) is NOT touched.
  $0 logs --replica I
        Tail the log for replica I (1-indexed).
  $0 status
        List all fleet replicas and their state.

Env overrides:
  FLEET_PREFIX        replica name prefix (default sentinel-sandbox-fleet)
  FLEET_LOG_DIR       log root (default /tmp/sentinel-sandbox/fleet)
  FLEET_PORT_BASE     first port of the workload-port grid (default 19000)
  FLEET_PORT_SLICE    ports per replica (default 10)

Per-replica behavior overrides (set before running 'up'):
  SENTINEL_REPO       host repo path (one worktree per replica is the
                       intended pattern for regression-search)
  SENTINEL_LLM_PREFER passed through to the container's sentinel binary
                       — see that binary's own docs for accepted values
EOF
}

require_docker() { command -v docker >/dev/null || die "docker not on PATH"; }

replica_name() { printf '%s-%02d' "$FLEET_PREFIX" "$1"; }

cmd_up() {
  local count=0
  local plan=""
  local detach=0
  while (( $# > 0 )); do
    case "$1" in
      --count) count="$2"; shift 2 ;;
      --plan)  plan="$2"; shift 2 ;;
      --detach) detach=1; shift ;;
      *) die "unknown arg: $1" ;;
    esac
  done
  (( count > 0 )) || die "--count must be > 0"
  [[ -n "$plan" ]] || die "--plan is required"
  [[ -f "$plan" ]] || die "plan not found: $plan"
  [[ "$plan" == /* ]] || die "plan must be absolute"

  # Verify the dev image is built. Fleet uses
  # `sentinel-sandbox-dev:local` directly via `docker run` rather
  # than through compose, so a fresh checkout needs the regular
  # `sandbox-up.sh` to have built the image at least once.
  docker image inspect sentinel-sandbox-dev:local >/dev/null 2>&1 \
    || die "sentinel-sandbox-dev:local image not built. run: bash $SCRIPT_DIR/sandbox-up.sh build"

  # Ensure the host metrics dir exists — fleet replicas bind it
  # the same way the regular sandbox-dev container does.
  mkdir -p "$HOME/.claude/sentinel/metrics"

  mkdir -p "$LOG_ROOT"

  for ((i=1; i<=count; i++)); do
    local name; name=$(replica_name "$i")
    local port_lo=$(( PORT_BASE + (i-1) * PORT_SLICE ))
    local port_hi=$(( port_lo + PORT_SLICE - 1 ))
    local log="$LOG_ROOT/${name}.log"
    say "replica $i/$count → $name  ports ${port_lo}-${port_hi}  log $log"

    # Per-replica volumes. Named so a follow-up `down` can find
    # them. cache + state are intentionally NOT shared with the
    # main sandbox-dev: fleet runs should not poison the daily-
    # driver container's caches.
    local cache_vol="${name}-cache"
    local state_vol="${name}-state"

    # Background the docker run so we can fan out across N
    # replicas quickly. The grind log is the source of truth;
    # docker run -d would return faster but lose the grind log
    # capture below.
    (
      docker run --rm -d \
        --name "$name" \
        --user "$(id -u):$(id -g)" \
        -e SENTINEL_CLAUDE_DIR=/workspace/.container-state/claude \
        -e SENTINEL_SANDBOX_PORT_RANGE="${port_lo}-${port_hi}" \
        -e SENTINEL_WORKSTREAM="fleet:${name}" \
        ${SENTINEL_LLM_PREFER:+-e SENTINEL_LLM_PREFER="$SENTINEL_LLM_PREFER"} \
        -p "127.0.0.1:${port_lo}-${port_hi}:${port_lo}-${port_hi}" \
        -v "${SENTINEL_REPO:-$REPO_ROOT}":/workspace/sentinel:rw \
        -v "${cache_vol}":/workspace/.container-cache \
        -v "${state_vol}":/workspace/.container-state \
        -v "${HOME}/.claude/.credentials.json":/workspace/.container-state/claude/.credentials.json:ro \
        -v "${HOME}/.claude/sentinel/metrics":/workspace/.container-state/claude/sentinel/metrics \
        -v "${SENTINEL_REPO:-$REPO_ROOT}/tools/sandbox/claude-settings-hooked.json":/workspace/.container-state/claude/settings.json:ro \
        -v "${HOME}/firefly/legatus-consul-agent":/workspace/legatus-consul-agent:rw \
        -v "${SENTINEL_REPO:-$REPO_ROOT}/tools/sandbox/sandbox-bootstrap.sh":/usr/local/bin/sandbox-bootstrap.sh:ro \
        -v "${plan}":/workspace/plan.md:ro \
        sentinel-sandbox-dev:local \
        bash -lc '
          /usr/local/bin/sandbox-bootstrap.sh >/dev/null 2>&1
          claude --print "Read /workspace/plan.md and execute it autonomously."
        ' >>"$log" 2>&1
    ) &
  done

  wait
  say ""
  say "fleet up: $count replicas. tail logs with:"
  say "  bash $0 logs --replica <i>"
  say "  bash $0 status"
}

cmd_down() {
  require_docker
  local names; names=$(docker ps -a --filter "name=^${FLEET_PREFIX}-" --format '{{.Names}}')
  if [[ -z "$names" ]]; then say "no fleet replicas running"; return 0; fi
  say "stopping fleet:"
  echo "$names"
  echo "$names" | xargs -r -n1 docker rm -f >/dev/null
  say "removing per-replica volumes"
  docker volume ls --format '{{.Name}}' | grep "^${FLEET_PREFIX}-" \
    | xargs -r -n1 docker volume rm >/dev/null
  say "done"
}

cmd_logs() {
  local replica=""
  while (( $# > 0 )); do
    case "$1" in
      --replica) replica="$2"; shift 2 ;;
      *) die "unknown arg: $1" ;;
    esac
  done
  [[ -n "$replica" ]] || die "--replica is required"
  local log="$LOG_ROOT/$(replica_name "$replica").log"
  [[ -f "$log" ]] || die "log not found: $log"
  tail -f "$log"
}

cmd_status() {
  require_docker
  docker ps -a --filter "name=^${FLEET_PREFIX}-" \
    --format 'table {{.Names}}\t{{.State}}\t{{.Status}}\t{{.Ports}}'
}

case "${1:-help}" in
  up)      shift; cmd_up "$@" ;;
  down)    shift; cmd_down "$@" ;;
  logs)    shift; cmd_logs "$@" ;;
  status)  shift; cmd_status "$@" ;;
  -h|--help|help) usage ;;
  *) usage; die "unknown subcommand: $1" ;;
esac
