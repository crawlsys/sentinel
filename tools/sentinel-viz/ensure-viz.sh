#!/usr/bin/env bash
# ensure-viz.sh — idempotent bring-up of the sentinel-viz bridge + HTTP server.
#
# Safe to call from a SessionStart hook (or anywhere on a hot path):
#   - returns immediately (<100ms) if both processes are already alive
#   - spawns them in the background if missing, then returns
#
# Honors $SENTINEL_VIZ_PORT (default 8081), $SENTINEL_VIZ_DISABLE=1 (skip),
# and $SENTINEL_VIZ_LOG_DIR (default /tmp).
set -euo pipefail

if [[ "${SENTINEL_VIZ_DISABLE:-0}" == "1" ]]; then
  exit 0
fi

PORT="${SENTINEL_VIZ_PORT:-8081}"
LOG_DIR="${SENTINEL_VIZ_LOG_DIR:-/tmp}"
# This script lives alongside the python files; resolve through symlinks
# (we're often called via ~/.local/bin/sentinel-ensure-viz → real path).
VIZ_DIR="$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")" && pwd)"

if [[ ! -f "$VIZ_DIR/viz_server.py" ]]; then
  echo "[sentinel-viz] viz_server.py not found at $VIZ_DIR — skipping" >&2
  exit 0
fi

# Pick the activegraph-equipped python interpreter
AG_PY="$HOME/.local/share/pipx/venvs/activegraph/bin/python"
if [[ ! -x "$AG_PY" ]]; then
  # Fall back to system python; the bridge will fail loudly if activegraph isn't importable.
  AG_PY="$(command -v python3 || true)"
fi
PY="$(command -v python3 || true)"
[[ -z "$PY" || -z "$AG_PY" ]] && { echo "[sentinel-viz] no python3 on PATH — skipping" >&2; exit 0; }

is_port_busy() {
  # Prefer ss; fall back to /dev/tcp probe
  if command -v ss >/dev/null 2>&1; then
    ss -ltn "sport = :$PORT" 2>/dev/null | grep -q LISTEN
  else
    (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null && { exec 3<&-; exec 3>&-; return 0; } || return 1
  fi
}

is_bridge_running() {
  pgrep -af 'sentinel_bridge\.py.*--tail' >/dev/null 2>&1
}

# --- viz server ---
if ! is_port_busy; then
  nohup "$PY" "$VIZ_DIR/viz_server.py" --port "$PORT" \
    </dev/null >"$LOG_DIR/sentinel-viz.log" 2>&1 &
  disown 2>/dev/null || true
fi

# --- bridge (live-tail) ---
if ! is_bridge_running; then
  nohup "$AG_PY" "$VIZ_DIR/sentinel_bridge.py" --tail \
    </dev/null >"$LOG_DIR/sentinel-bridge.log" 2>&1 &
  disown 2>/dev/null || true
fi

exit 0
