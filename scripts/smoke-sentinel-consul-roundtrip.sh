#!/usr/bin/env bash
# Shell-level smoke test: drive a real `sentinel daemon` against a
# real `consulate` over a real WebSocket on ephemeral ports, and
# assert the daemon's `GET /legatus/health` endpoint reports
# `connected` once the handshake completes.
#
# This is the smallest end-to-end proof that the two binaries
# actually talk to each other on the wire, beyond the in-process
# round-trip Rust tests. Useful for catching ABI-shaped issues
# (transport quirks, env handling, working-dir defaults, token
# files) that unit tests can't reach.
#
# Prerequisites:
#   - Release builds of both binaries:
#       cargo build --release -p sentinel
#       (cd ../legatus-consul-agent && cargo build --release -p consulate)
#   - jq for JSON parsing
#   - lsof for port-binding waits
#
# Usage:
#   ./scripts/smoke-sentinel-consul-roundtrip.sh
#
# Exits 0 on success, non-zero on any failure (with a message).
# Always cleans up its background processes via the EXIT trap.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONSUL_REPO="${CONSUL_REPO:-${REPO_ROOT}/../legatus-consul-agent}"

SENTINEL_BIN="${SENTINEL_BIN:-${REPO_ROOT}/target/release/sentinel}"
CONSULATE_BIN="${CONSULATE_BIN:-${CONSUL_REPO}/target/release/consulate}"

# 64-char hex (32 bytes) shared between consulate and the daemon's
# legatus. Test-only value — never reuse in any real deployment.
BOOTSTRAP_SECRET="$(printf '77%.0s' {1..32})"

# Ephemeral ports — let the OS pick by binding+release. The brief
# window between release and re-bind is racy in theory but fine for
# a serial smoke test.
pick_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

CONSULATE_PORT="$(pick_port)"
DAEMON_PORT="$(pick_port)"

# Sentinel writes its bearer token to ~/.claude/sentinel/daemon-token
# as "port:token". Read it AFTER the daemon starts.
TOKEN_FILE="${HOME}/.claude/sentinel/daemon-token"

CONSULATE_LOG="$(mktemp -t consulate.XXXXXX.log)"
SENTINEL_LOG="$(mktemp -t sentinel.XXXXXX.log)"
TEST_DB="$(mktemp -t consul.XXXXXX.db)"

CONSULATE_PID=""
SENTINEL_PID=""

cleanup() {
    set +e
    if [[ -n "${SENTINEL_PID}" ]]; then
        kill "${SENTINEL_PID}" 2>/dev/null
        wait "${SENTINEL_PID}" 2>/dev/null
    fi
    if [[ -n "${CONSULATE_PID}" ]]; then
        kill "${CONSULATE_PID}" 2>/dev/null
        wait "${CONSULATE_PID}" 2>/dev/null
    fi
    rm -f "${TEST_DB}"
    if [[ "${KEEP_LOGS:-0}" != "1" ]]; then
        rm -f "${CONSULATE_LOG}" "${SENTINEL_LOG}"
    else
        echo "  consulate log: ${CONSULATE_LOG}"
        echo "  sentinel log:  ${SENTINEL_LOG}"
    fi
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    echo "--- consulate log ---" >&2
    tail -40 "${CONSULATE_LOG}" >&2 || true
    echo "--- sentinel log ---" >&2
    tail -40 "${SENTINEL_LOG}" >&2 || true
    exit 1
}

# Wait until `lsof -i :PORT` reports LISTEN, or time out.
wait_for_port() {
    local port="$1" label="$2" budget_s="${3:-10}"
    local deadline=$(( $(date +%s) + budget_s ))
    while (( $(date +%s) < deadline )); do
        if lsof -nP -iTCP:"${port}" -sTCP:LISTEN >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    fail "${label} did not bind port ${port} within ${budget_s}s"
}

# Poll the bearer-authed /legatus/health route until status==expected
# or time out.
wait_for_health_status() {
    local expected="$1" budget_s="${2:-10}"
    local deadline=$(( $(date +%s) + budget_s ))
    local last=""
    while (( $(date +%s) < deadline )); do
        last="$(curl -fsS -H "Authorization: Bearer ${TOKEN}" \
            "http://127.0.0.1:${DAEMON_PORT}/legatus/health" 2>/dev/null \
            | jq -r '.status' 2>/dev/null || echo "")"
        if [[ "${last}" == "${expected}" ]]; then
            return 0
        fi
        sleep 0.2
    done
    fail "expected /legatus/health status='${expected}', last observed='${last}' (timeout ${budget_s}s)"
}

# Sanity-check the binaries exist before doing anything that takes
# time. The user-facing error is much friendlier here than after a
# 60s timeout.
[[ -x "${SENTINEL_BIN}" ]] \
    || fail "sentinel binary not found at ${SENTINEL_BIN}; run 'cargo build --release -p sentinel' first"
[[ -x "${CONSULATE_BIN}" ]] \
    || fail "consulate binary not found at ${CONSULATE_BIN}; build it in ${CONSUL_REPO}"
command -v jq >/dev/null || fail "jq not installed"
command -v lsof >/dev/null || fail "lsof not installed"

echo "==> Starting consulate on 127.0.0.1:${CONSULATE_PORT}"
"${CONSULATE_BIN}" \
    --bind "127.0.0.1:${CONSULATE_PORT}" \
    --insecure-localhost-only \
    --bootstrap-secret "${BOOTSTRAP_SECRET}" \
    --db-url "sqlite::memory:" \
    >"${CONSULATE_LOG}" 2>&1 &
CONSULATE_PID=$!
wait_for_port "${CONSULATE_PORT}" "consulate" 10

echo "==> Starting sentinel daemon on 127.0.0.1:${DAEMON_PORT}"
"${SENTINEL_BIN}" daemon \
    --port "${DAEMON_PORT}" \
    --legatus-consulate-url "ws://127.0.0.1:${CONSULATE_PORT}" \
    --legatus-bootstrap-secret "${BOOTSTRAP_SECRET}" \
    --legatus-suggested-name "smoke-test" \
    --legatus-heartbeat-secs 1 \
    >"${SENTINEL_LOG}" 2>&1 &
SENTINEL_PID=$!
wait_for_port "${DAEMON_PORT}" "sentinel daemon" 10

# Read the bearer token written by the daemon. Wait briefly for the
# file (the daemon writes it after binding but before serving).
for _ in $(seq 1 50); do
    [[ -f "${TOKEN_FILE}" ]] && break
    sleep 0.1
done
[[ -f "${TOKEN_FILE}" ]] || fail "daemon never wrote ${TOKEN_FILE}"
# Format is "port:token" — strip the port prefix matching ours.
TOKEN_LINE="$(cat "${TOKEN_FILE}")"
TOKEN="${TOKEN_LINE#*:}"
[[ -n "${TOKEN}" ]] || fail "daemon-token file present but empty"

echo "==> Polling /legatus/health until status=connected"
wait_for_health_status "connected" 15
echo "    -> connected"

echo "==> Killing consulate to verify reconnect transitions"
kill "${CONSULATE_PID}"
wait "${CONSULATE_PID}" 2>/dev/null || true
CONSULATE_PID=""

# The daemon should detect the dropped WS within a heartbeat
# interval (~1s) and transition to reconnecting. Allow generous
# slack for slow CI / heartbeat skew.
wait_for_health_status "reconnecting" 10
echo "    -> reconnecting"

echo "==> Restarting consulate to verify reconnect succeeds"
"${CONSULATE_BIN}" \
    --bind "127.0.0.1:${CONSULATE_PORT}" \
    --insecure-localhost-only \
    --bootstrap-secret "${BOOTSTRAP_SECRET}" \
    --db-url "sqlite::memory:" \
    >>"${CONSULATE_LOG}" 2>&1 &
CONSULATE_PID=$!
wait_for_port "${CONSULATE_PORT}" "consulate (restart)" 10
wait_for_health_status "connected" 35
echo "    -> reconnected"

echo "PASS: sentinel <-> consul roundtrip + reconnect verified"
