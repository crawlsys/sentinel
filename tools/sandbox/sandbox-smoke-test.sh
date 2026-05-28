#!/usr/bin/env bash
# sandbox-smoke-test.sh — verify the sandbox stack is wired
# end-to-end. Run after `sandbox-up.sh` reports the stack is up.
#
# Checks, in dependency order:
#   1. viz-api is listening on :8082 and returns 200 on /api/healthz.
#   2. viz-next is listening on :8083 and returns a page that
#      mentions "sentinel-viz".
#   3. sentinel-bridge container is running and tailing.
#   4. The container's sandbox-dev can write a synthetic hook
#      event, the bridge ingests it within ~10s, and viz-api's
#      graph endpoint shows the synthetic SentinelHookInvocation.
#
# Exits 0 on all green; non-zero on the first failure, with a
# specific diagnostic. This is the "did the wiring actually
# connect" test the existing COMPOSE-INTEGRATION.md walks through
# manually.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPOSE_FILE="$SCRIPT_DIR/docker-compose.yml"

VIZ_API="${VIZ_API:-http://localhost:8082}"
VIZ_NEXT="${VIZ_NEXT:-http://localhost:8083}"

# Synthetic event timeout: how long to wait for the bridge to
# ingest a freshly-written hook. Empirically ~5s; we give it
# 15 to absorb cold-start slowness.
INGEST_TIMEOUT_S=15

ok()   { printf '\033[1;32m[ ok ]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[fail]\033[0m %s\n' "$*" >&2; exit 1; }
say()  { printf '\033[1;36m[smoke]\033[0m %s\n' "$*"; }

say "1/4 — viz-api healthz"
http_code=$(curl -fsS -o /dev/null -w '%{http_code}' --max-time 5 "$VIZ_API/api/healthz" || echo "000")
[[ "$http_code" == "200" ]] || fail "viz-api /api/healthz returned $http_code (expected 200) — is the container up? docker compose ps"
ok "viz-api responded 200"

say "2/4 — viz-next index"
page=$(curl -fsS --max-time 5 "$VIZ_NEXT/" || true)
echo "$page" | grep -q "sentinel-viz" || fail "viz-next index page didn't mention 'sentinel-viz' — bundle build may have failed; check 'docker compose logs viz-next'"
ok "viz-next is serving the dashboard"

say "3/4 — bridge container running"
bridge_state=$(docker compose -f "$COMPOSE_FILE" ps -q sentinel-bridge 2>/dev/null || true)
[[ -n "$bridge_state" ]] || fail "sentinel-bridge container is not running — 'docker compose logs sentinel-bridge'"
running=$(docker inspect -f '{{.State.Running}}' "$bridge_state" 2>/dev/null || echo "false")
[[ "$running" == "true" ]] || fail "sentinel-bridge container is not in running state"
ok "sentinel-bridge is running"

say "4/4 — synthetic hook ingestion"
# Write a synthetic SessionStart event into the host's
# metrics dir — same dir the bridge tails AND the sandbox-dev
# container writes to via its bind-mount. The bridge should
# pick it up within the tail-poll interval.
METRICS_DIR="${HOME}/.claude/sentinel/metrics"
[[ -d "$METRICS_DIR" ]] || fail "host metrics dir missing: $METRICS_DIR — did sandbox-up.sh run?"

SYNTHETIC_SESSION="smoke-$(date +%s)"
SYNTHETIC_TS="$(date -u +%Y-%m-%dT%H:%M:%S)"
SYNTHETIC_LINE="$(printf '{"session_id":"%s","timestamp":"%s","event":"SessionStart","hook":"smoke-test","outcome":"allow","source_harness":"claude"}' \
    "$SYNTHETIC_SESSION" "$SYNTHETIC_TS")"
echo "$SYNTHETIC_LINE" >> "$METRICS_DIR/hook-invocations.jsonl"
say "  wrote synthetic event for session $SYNTHETIC_SESSION"

deadline=$(( $(date +%s) + INGEST_TIMEOUT_S ))
found=0
while (( $(date +%s) < deadline )); do
  if curl -fsS --max-time 3 "$VIZ_API/api/graph?include_hooks=true&limit=200" 2>/dev/null \
       | grep -q "$SYNTHETIC_SESSION"; then
    found=1
    break
  fi
  sleep 1
done
(( found == 1 )) || fail "bridge did not ingest synthetic event within ${INGEST_TIMEOUT_S}s — check 'docker compose logs sentinel-bridge'"
ok "bridge ingested synthetic event (session $SYNTHETIC_SESSION)"

say ""
say "all checks passed — stack is wired correctly"
