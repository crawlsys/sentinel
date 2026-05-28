#!/usr/bin/env bash
# sandbox-down.sh — stop the sentinel sandbox stack.
#
# Default: `down` (stop + remove the container, keep volumes —
# so a follow-up `sandbox-up.sh` is a hot start: no rebuild,
# cargo cache + claude state survive).
#
# Pass `--purge` to drop volumes too. Destructive: deletes
# sandbox-cache (cargo / pnpm artifacts, ~3min to rebuild) and
# sandbox-state (claude session history, in-container worktrees).
# Hook output written to the host's ~/.claude/sentinel/metrics/
# is OUTSIDE these volumes and is never touched by `--purge`.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"

say() { printf '\033[1;36m[sandbox-down]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[sandbox-down][ERR]\033[0m %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null || die "docker not on PATH"

case "${1:-down}" in
  down|"")
    say "stopping stack (keeping volumes)"
    docker compose -f "$COMPOSE_FILE" down
    ;;
  --purge|purge)
    say "stopping stack AND dropping volumes"
    say "this deletes sandbox-cache + sandbox-state"
    read -r -p "are you sure? [y/N] " ans
    [[ "$ans" =~ ^[Yy]$ ]] || { say "aborted"; exit 0; }
    docker compose -f "$COMPOSE_FILE" down -v
    ;;
  *)
    die "unknown subcommand: $1 (expected '' or '--purge')"
    ;;
esac
