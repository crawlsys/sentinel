#!/usr/bin/env bash
# sandbox-grind.sh — autonomous claude run inside the sandbox
# against a plan file. Streams output to a log under
# /tmp/sentinel-sandbox/grinds/, logs the session_id, exits with
# claude's exit code.
#
# Usage:
#   bash tools/sandbox/sandbox-grind.sh /abs/path/to/plan.md
#   bash tools/sandbox/sandbox-grind.sh /abs/path/to/plan.md --name my-grind
#
# Expects the sandbox stack to already be up (sandbox-up.sh).
# Uses the existing sandbox-dev container; one grind at a time
# per container. For parallel grinds, see sandbox-fleet.sh.
#
# Why this exists (vs operator running `claude` interactively in
# the container): autonomous mode + a fixed plan file is the
# pattern we want for benchmarking / regression-running / "let
# it cook overnight" workflows. The grind is detachable
# (--detach) and the log is structured so a later run can grep
# tool counts / outcomes from it.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"

LOG_ROOT="${SENTINEL_GRIND_LOG_DIR:-/tmp/sentinel-sandbox/grinds}"
GRIND_NAME=""
DETACH=0
PLAN=""

say() { printf '\033[1;36m[grind]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[grind][ERR]\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<EOF
usage: $0 <plan.md> [--name NAME] [--detach]

  <plan.md>      Absolute path to a plan file (host path).
                 Bind-mounted RO at /workspace/plan.md inside.
  --name NAME    Tag for the log file. Default: derived from
                 plan basename + timestamp.
  --detach       Run in background, print PID + log path, exit
                 immediately. Useful for long-running grinds /
                 fleet runs.
EOF
}

while (( $# > 0 )); do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --name) GRIND_NAME="$2"; shift 2 ;;
    --detach) DETACH=1; shift ;;
    *)
      if [[ -z "$PLAN" ]]; then
        PLAN="$1"
        shift
      else
        die "unexpected arg: $1 (already have plan=$PLAN). See --help."
      fi
      ;;
  esac
done

[[ -n "$PLAN" ]] || { usage; die "no plan file given"; }
[[ -f "$PLAN" ]] || die "plan file not found: $PLAN"
[[ "$PLAN" == /* ]] || die "plan path must be absolute: $PLAN"

command -v docker >/dev/null || die "docker not on PATH"

container_state=$(docker inspect -f '{{.State.Running}}' sentinel-sandbox-dev 2>/dev/null || echo "missing")
[[ "$container_state" == "true" ]] || die "sentinel-sandbox-dev not running — bring the stack up first:
  bash $SCRIPT_DIR/sandbox-up.sh"

if [[ -z "$GRIND_NAME" ]]; then
  base=$(basename "$PLAN" .md)
  GRIND_NAME="${base}-$(date +%Y%m%d-%H%M%S)"
fi

mkdir -p "$LOG_ROOT"
LOG_FILE="$LOG_ROOT/$GRIND_NAME.log"

# Mount the plan in at /workspace/plan.md so the in-container
# claude can `Read` it. Then drive claude --print with a tight
# bootstrap prompt that points it at the plan. Hooks fire normally
# (claude-settings-hooked.json is already mounted by the compose),
# so the grind shows up in the dashboard as a real session.
GRIND_PROMPT='Read /workspace/plan.md and execute it autonomously. \
Follow the plan in order, stop at explicit decision-gates, and \
exit when the plan reports done. Use Sentinel tools as needed.'

# The docker cp + exec dance keeps the plan path mount-free —
# the existing sandbox-dev compose definition doesn't include a
# generic plan-mount slot, and editing the compose per-grind is
# brittle. Instead, copy the plan into /workspace/plan.md
# inside the container, run, then clean up.
say "grind:    $GRIND_NAME"
say "plan:     $PLAN"
say "log:      $LOG_FILE"

run_grind() {
  docker cp "$PLAN" sentinel-sandbox-dev:/workspace/plan.md
  trap 'docker exec sentinel-sandbox-dev rm -f /workspace/plan.md 2>/dev/null || true' EXIT
  docker exec -i \
    -e SENTINEL_WORKSTREAM="grind:$GRIND_NAME" \
    sentinel-sandbox-dev \
    bash -lc "claude --print '$GRIND_PROMPT'"
}

if (( DETACH == 1 )); then
  ( run_grind >"$LOG_FILE" 2>&1 ) &
  PID=$!
  say "detached. pid=$PID  tail with: tail -f $LOG_FILE"
  exit 0
else
  run_grind 2>&1 | tee "$LOG_FILE"
fi
