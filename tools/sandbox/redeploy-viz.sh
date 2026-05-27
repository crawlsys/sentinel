#!/usr/bin/env bash
# Rebuild + restart the sentinel-viz stack (viz-api + viz-next).
#
# Run this on the host where the viz processes live (172.16.100.22
# in the current deployment). It assumes the worktree at
# `$WORKTREE` is already on the desired branch — does NOT git pull
# (deliberate: lets the operator stage / test commits before
# redeploying).
#
# What it does, in order:
#   1. Build viz-api (Rust): `cargo build --release -p sentinel-viz-api`.
#      Pulls in the unified LlmRouter so naming/summary calls
#      route through SENTINEL_LLM_PREFER + OLLAMA_HOST.
#   2. Build viz-next (Next.js): `pnpm install && pnpm build`.
#      Compiles latest EventTicker / SessionStrip / domain code
#      into the static bundle that `next start` serves.
#   3. Stop any existing viz-api + viz-next processes (PID files
#      under /tmp/sentinel-viz/).
#   4. Start them back up with the right env wired in.
#
# Idempotent: re-running just rebuilds + bounces. No state is
# lost (the SQLite store + activity cache live outside this
# script's scope).

set -euo pipefail

WORKTREE="${SENTINEL_WORKTREE:-/home/kcrawley/firefly/sentinel/.claude/worktrees/sentinel-viz-tweaks}"
PID_DIR="${SENTINEL_VIZ_PID_DIR:-/tmp/sentinel-viz}"
LOG_DIR="${SENTINEL_VIZ_LOG_DIR:-/tmp/sentinel-viz/logs}"

# AI routing config. Defaults wire viz-api at the operator's
# nighttime box (RTX 6000 Pro Blackwell @ 172.16.100.125) via the
# ollama-research NodePort. Override by exporting before
# invocation.
export SENTINEL_LLM_PREFER="${SENTINEL_LLM_PREFER:-local}"
export OLLAMA_HOST="${OLLAMA_HOST:-http://172.16.100.125:31435}"
export OLLAMA_MODEL="${OLLAMA_MODEL:-qwen3-coder:30b}"

# Frontend → backend URL. Baked into the client bundle at build
# time; must match where viz-api actually listens.
export NEXT_PUBLIC_VIZ_API="${NEXT_PUBLIC_VIZ_API:-http://172.16.100.22:8082}"

# Where viz-api binds.
export VIZ_API_HOST="${VIZ_API_HOST:-0.0.0.0}"
export VIZ_API_PORT="${VIZ_API_PORT:-8082}"
export VIZ_NEXT_PORT="${VIZ_NEXT_PORT:-3000}"

mkdir -p "$PID_DIR" "$LOG_DIR"

say() { printf '\033[1;36m[redeploy-viz]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[redeploy-viz][ERR]\033[0m %s\n' "$*" >&2; exit 1; }

[[ -d "$WORKTREE" ]] || die "worktree not found: $WORKTREE (set SENTINEL_WORKTREE)"
cd "$WORKTREE"

say "worktree: $WORKTREE"
say "branch:   $(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo '(detached)')"
say "head:     $(git rev-parse --short HEAD)"
say "ai:       SENTINEL_LLM_PREFER=$SENTINEL_LLM_PREFER OLLAMA_HOST=$OLLAMA_HOST OLLAMA_MODEL=$OLLAMA_MODEL"

# --- 1. Build viz-api ----------------------------------------------------------

say "[1/4] building viz-api (release)"
# sentinel-viz-api is a STANDALONE workspace (tools/sentinel-viz-api
# has its own [workspace] in Cargo.toml; the parent's workspace
# excludes it). Build from inside the sub-workspace so cargo can
# find the package.
(cd "$WORKTREE/tools/sentinel-viz-api" && cargo build --release) 2>&1 | tail -20 \
    || die "cargo build failed"

# Locate the built binary. Cargo workspaces may put it at the
# workspace target dir; sentinel-viz-api uses its own workspace.
VIZ_API_BIN=""
for candidate in \
  "$WORKTREE/target/release/sentinel-viz-api" \
  "$WORKTREE/tools/sentinel-viz-api/target/release/sentinel-viz-api"; do
  [[ -x "$candidate" ]] && { VIZ_API_BIN="$candidate"; break; }
done
[[ -n "$VIZ_API_BIN" ]] || die "viz-api binary not found after build"
say "      → $VIZ_API_BIN"

# --- 2. Build viz-next ---------------------------------------------------------

say "[2/4] building viz-next"
cd "$WORKTREE/tools/sentinel-viz-next"
if [[ ! -d node_modules ]] || [[ package.json -nt node_modules ]]; then
  pnpm install --frozen-lockfile 2>&1 | tail -5 || die "pnpm install failed"
fi
pnpm build 2>&1 | tail -10 || die "pnpm build failed"

# --- 3. Stop existing processes -----------------------------------------------

say "[3/4] stopping running processes"
for svc in viz-api viz-next; do
  pidfile="$PID_DIR/$svc.pid"
  if [[ -f "$pidfile" ]] && pid=$(cat "$pidfile") && kill -0 "$pid" 2>/dev/null; then
    say "      stopping $svc (pid $pid)"
    kill "$pid"
    # Wait up to 10s for graceful shutdown
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$pid" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$pidfile"
done

# Also catch processes started outside this script (e.g. the
# 17:41 pnpm start mentioned in the operator's process listing
# that pre-dates the PID-file convention).
say "      sweeping orphans on :$VIZ_API_PORT / :$VIZ_NEXT_PORT"
for port in "$VIZ_API_PORT" "$VIZ_NEXT_PORT"; do
  if command -v fuser >/dev/null; then
    fuser -k "$port/tcp" 2>/dev/null || true
  elif command -v lsof >/dev/null; then
    pids=$(lsof -ti ":$port" 2>/dev/null || true)
    [[ -n "$pids" ]] && kill $pids 2>/dev/null || true
  fi
done
sleep 1

# --- 4. Start them back up -----------------------------------------------------

say "[4/4] starting viz-api + viz-next"
cd "$WORKTREE"

# viz-api: the LlmRouter probes OLLAMA_HOST at startup and logs
# its routing decision so the operator can verify what got picked.
nohup env \
  SENTINEL_LLM_PREFER="$SENTINEL_LLM_PREFER" \
  OLLAMA_HOST="$OLLAMA_HOST" \
  OLLAMA_MODEL="$OLLAMA_MODEL" \
  RUST_LOG="${RUST_LOG:-sentinel_viz_api=info,sentinel::llm_router=info}" \
  "$VIZ_API_BIN" \
  --host "$VIZ_API_HOST" --port "$VIZ_API_PORT" \
  > "$LOG_DIR/viz-api.log" 2>&1 &
echo $! > "$PID_DIR/viz-api.pid"
say "      viz-api  pid=$(cat $PID_DIR/viz-api.pid)  log=$LOG_DIR/viz-api.log"

cd "$WORKTREE/tools/sentinel-viz-next"
nohup env \
  NEXT_PUBLIC_VIZ_API="$NEXT_PUBLIC_VIZ_API" \
  pnpm start -p "$VIZ_NEXT_PORT" -H 0.0.0.0 \
  > "$LOG_DIR/viz-next.log" 2>&1 &
echo $! > "$PID_DIR/viz-next.pid"
say "      viz-next pid=$(cat $PID_DIR/viz-next.pid)  log=$LOG_DIR/viz-next.log"

sleep 2
say "done. health:"
say "  viz-api:  $(curl -sS -o /dev/null -w '%{http_code} in %{time_total}s' "http://localhost:$VIZ_API_PORT/api/health" 2>&1 || echo unreachable)"
say "  viz-next: $(curl -sS -o /dev/null -w '%{http_code} in %{time_total}s' "http://localhost:$VIZ_NEXT_PORT/" 2>&1 || echo unreachable)"
say ""
say "first 30 lines of viz-api log (look for 'llm_router' routing decision):"
sed -n '1,30p' "$LOG_DIR/viz-api.log" 2>/dev/null || true
