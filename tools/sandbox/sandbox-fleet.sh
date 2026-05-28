#!/usr/bin/env bash
# sandbox-fleet.sh — spin N parallel sandbox-dev containers
# against a plan file. The "I hate money" surface: each replica
# is a fully-isolated container with its own cache + state
# volumes, so you can throw 4-16 of them at the same plan and
# benchmark variance / regression-search / model A-B comparisons
# without containers stepping on each other.
#
# What it's good for:
#   - Benchmarking: same plan against multiple model versions
#     (set OLLAMA_MODEL or SENTINEL_LLM_PREFER per replica).
#   - Regression hunt: same plan against multiple sentinel
#     commits (set SENTINEL_REPO per replica to a different
#     worktree).
#   - Stress-test the bridge ingest path: 16 hookful grinds
#     writing to the same metrics dir validates the bridge's
#     per-session deduplication.
#   - Skill / hook A/B: half the fleet runs with hooks on, half
#     with hooks off (claude-settings-hookless.json).
#
# What it's NOT good for:
#   - Cheap exploratory work — the cargo cache is per-replica
#     unless you share SANDBOX_CACHE_VOL, so first boot is
#     ~3min × N.
#   - Anything sensitive to host port collisions — the agent
#     workload port range gets carved into N stripes so each
#     replica has its own slice.
#
# Usage:
#   bash tools/sandbox/sandbox-fleet.sh up   --count 4 --plan /abs/plan.md
#   bash tools/sandbox/sandbox-fleet.sh logs --replica 2
#   bash tools/sandbox/sandbox-fleet.sh down
#
# Each replica is named sentinel-sandbox-fleet-<i>; viz-api +
# viz-next + sentinel-bridge are SHARED across the fleet (one
# stack-level instance each) since the bridge is the central
# observability sink.
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
        Stop and remove all fleet replicas. The shared
        viz/bridge stack is NOT touched (use sandbox-down.sh).
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
  SENTINEL_LLM_PREFER local | cloud | auto
  OLLAMA_HOST         override the unified LLM router target
  OLLAMA_MODEL        override the active model name
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

  # Verify the shared stack is up. The bridge is the load-bearing
  # piece — without it, the replicas produce metrics nothing
  # consumes.
  docker inspect -f '{{.State.Running}}' sentinel-bridge 2>/dev/null | grep -q true \
    || die "shared stack not up. run: bash $SCRIPT_DIR/sandbox-up.sh"

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
        --network "sentinel-sandbox_default" \
        --user "$(id -u):$(id -g)" \
        -e SENTINEL_CLAUDE_DIR=/workspace/.container-state/claude \
        -e SENTINEL_SANDBOX_PORT_RANGE="${port_lo}-${port_hi}" \
        -e SENTINEL_WORKSTREAM="fleet:${name}" \
        ${SENTINEL_LLM_PREFER:+-e SENTINEL_LLM_PREFER="$SENTINEL_LLM_PREFER"} \
        ${OLLAMA_HOST:+-e OLLAMA_HOST="$OLLAMA_HOST"} \
        ${OLLAMA_MODEL:+-e OLLAMA_MODEL="$OLLAMA_MODEL"} \
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
