#!/usr/bin/env bash
# sandbox-smoke-test.sh — verify the sandbox container is wired
# correctly. Run after `sandbox-up.sh` reports the container is up.
#
# Checks, in dependency order:
#   1. sandbox-dev container is running.
#   2. The container has `sentinel`, `claude`, and `git` on PATH
#      (bootstrap completed successfully).
#   3. The container can write to its metrics directory and the
#      write surfaces at the host's ~/.claude/sentinel/metrics/
#      (the cross-boundary bind-mount is wired).
#   4. The agent-workload port range is forwarded: bind a port
#      inside the container, hit it from the host.
#
# Exits 0 on all green; non-zero on the first failure, with a
# specific diagnostic. NO viz/dashboard checks — those belong
# to the viz workstream; this smoke test cares only about
# "the sandbox container is functional and the seam to the
# host is open."

set -euo pipefail

CTR="${SENTINEL_SANDBOX_CTR:-sentinel-sandbox-dev}"
PORT_PROBE=${SANDBOX_SMOKE_PROBE_PORT:-18099}  # high end of the range, unlikely to clash with agent work

ok()   { printf '\033[1;32m[ ok ]\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31m[fail]\033[0m %s\n' "$*" >&2; exit 1; }
say()  { printf '\033[1;36m[smoke]\033[0m %s\n' "$*"; }

command -v docker >/dev/null || fail "docker not on PATH"

say "1/4 — container is running"
state=$(docker inspect -f '{{.State.Running}}' "$CTR" 2>/dev/null || echo "missing")
[[ "$state" == "true" ]] || fail "container '$CTR' is not running — bring it up: bash tools/sandbox/sandbox-up.sh"
ok  "$CTR is running"

say "2/4 — required binaries on PATH"
# Use `bash -lc` so /etc/profile.d/ + ~/.bashrc are sourced —
# matches what a real exec session sees.
missing=$(docker exec "$CTR" bash -lc 'for b in sentinel claude git; do command -v "$b" >/dev/null || echo "$b"; done')
if [[ -n "$missing" ]]; then
  fail "missing binaries in container: $missing
  - sentinel: bootstrap's cargo install failed; check 'docker logs $CTR'
  - claude:   Claude Code CLI install failed in the Dockerfile
  - git:      base image is broken, this should never happen"
fi
ok  "sentinel + claude + git all reachable"

say "3/4 — metrics bind-mount round-trips to the host"
PROBE_SESSION="smoke-$(date +%s)-$$"
PROBE_LINE="$(printf '{"session_id":"%s","event":"sandbox-smoke","ts":"%s"}' \
    "$PROBE_SESSION" "$(date -u +%Y-%m-%dT%H:%M:%SZ)")"
docker exec "$CTR" bash -lc \
  "printf '%s\n' '$PROBE_LINE' >> \$SENTINEL_CLAUDE_DIR/sentinel/metrics/sandbox-smoke.jsonl"

HOST_FILE="${HOME}/.claude/sentinel/metrics/sandbox-smoke.jsonl"
if [[ ! -f "$HOST_FILE" ]] || ! grep -q "$PROBE_SESSION" "$HOST_FILE"; then
  fail "container wrote to its metrics dir but the host can't see it at $HOST_FILE
  - the bind-mount in docker-compose.yml is wrong, or
  - the host's ~/.claude/sentinel/metrics/ doesn't exist (sandbox-up.sh creates it)"
fi
ok  "metrics dir bind-mount is wired (probe session $PROBE_SESSION visible host-side)"
# Cleanup the probe entry — leave the file intact since the
# host bridge may already have tailed it; just delete this run's
# line.
sed -i.bak "/$PROBE_SESSION/d" "$HOST_FILE" && rm -f "${HOST_FILE}.bak"

say "4/4 — agent-workload port range is forwarded"
# Bind a one-shot python server inside the container on the
# probe port, hit it from the host, kill it. Uses python because
# it's in the dev image and doesn't require an extra package.
docker exec -d "$CTR" bash -lc \
  "python3 -m http.server $PORT_PROBE --bind 0.0.0.0 >/tmp/smoke-http.log 2>&1 || true"
# Give the server a moment to bind.
for _ in 1 2 3 4 5; do
  http_code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 1 "http://127.0.0.1:$PORT_PROBE/" || echo "000")
  [[ "$http_code" =~ ^[23] ]] && break
  sleep 1
done
docker exec "$CTR" bash -lc "pkill -f 'http.server $PORT_PROBE' 2>/dev/null || true" >/dev/null 2>&1 || true
[[ "$http_code" =~ ^[23] ]] || fail "host couldn't reach the container's :$PORT_PROBE — agent-workload port range not forwarded properly"
ok  "host reached container on :$PORT_PROBE (HTTP $http_code)"

say ""
say "all checks passed — sandbox container is functional"
