#!/usr/bin/env bash
# sandbox-up.sh — bring up the sentinel sandbox stack.
#
# Daily-driver entrypoint. Wraps `docker compose -f
# tools/sandbox/docker-compose.yml up -d` with two ergonomics:
#
#   1. HOST_UID/HOST_GID are exported so the sandbox-dev image
#      builds with a UID matching the operator's host user —
#      keeps bind-mounted files writable.
#   2. The required host directories
#      (~/.claude/.credentials.json, ~/.claude/sentinel/metrics,
#      ~/firefly/legatus-consul-agent) are checked first.
#      Bind-mounts against missing paths produce a confusing
#      "directory mounted as file" error 30s into the build.
#
# Usage:
#   bash tools/sandbox/sandbox-up.sh         # default: up -d
#   bash tools/sandbox/sandbox-up.sh build   # force rebuild then up -d
#
# Tear down with sandbox-down.sh. Verify with sandbox-smoke-test.sh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

say()  { printf '\033[1;36m[sandbox-up]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[sandbox-up]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[sandbox-up][ERR]\033[0m %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null || die "docker not on PATH"

export SENTINEL_REPO="${SENTINEL_REPO:-$REPO_ROOT}"
export HOST_UID="${HOST_UID:-$(id -u)}"
export HOST_GID="${HOST_GID:-$(id -g)}"

# Pre-flight: the compose bind-mounts assume these exist on the
# host. Create the ones that are safe to create empty (metrics
# dir); error on the ones that aren't (credentials, sibling repo).
REQUIRED_BINDS=(
  "$HOME/.claude/.credentials.json"
  "$HOME/firefly/legatus-consul-agent"
)
CREATE_IF_MISSING=(
  "$HOME/.claude/sentinel/metrics"
)
for path in "${CREATE_IF_MISSING[@]}"; do
  mkdir -p "$path"
done
for path in "${REQUIRED_BINDS[@]}"; do
  if [[ ! -e "$path" ]]; then
    die "host path missing: $path
  - .credentials.json: log in to Claude Code on the host first (~/.claude/.credentials.json)
  - legatus-consul-agent: clone alongside (~/firefly/legatus-consul-agent)"
  fi
done

say "repo:    $SENTINEL_REPO"
say "compose: $COMPOSE_FILE"
say "uid/gid: $HOST_UID/$HOST_GID"

case "${1:-up}" in
  build)
    say "rebuilding all service images"
    docker compose -f "$COMPOSE_FILE" build
    docker compose -f "$COMPOSE_FILE" up -d
    ;;
  up|"")
    docker compose -f "$COMPOSE_FILE" up -d
    ;;
  *)
    die "unknown subcommand: $1 (expected 'up' or 'build')"
    ;;
esac

say ""
say "container up. agent-workload ports: http://localhost:18000..18099"
say ""
say "exec into the dev container:"
say "  docker compose -f $COMPOSE_FILE exec sandbox-dev bash"
say ""
say "verify the wiring:"
say "  bash $SCRIPT_DIR/sandbox-smoke-test.sh"
