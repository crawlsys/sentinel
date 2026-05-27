#!/usr/bin/env bash
# sandbox-bootstrap.sh — install the sentinel binary inside the
# sandbox container so `claude` hooks can dispatch
# `sentinel hook --event <Name>`.
#
# Idempotent: subsequent runs no-op when the binary is already
# present in the cached cargo-bin directory under the named state
# volume. First run costs ~3 minutes for the cargo install; cached
# runs return in <1s.
#
# Invoked at container start by sandbox-compose-override.yml's
# command: directive.

set -euo pipefail

SBIN="${SBIN:-/workspace/.container-state/cargo-bin/bin}"
SENTINEL_REPO="${SENTINEL_REPO:-/workspace/sentinel}"
CONSUL_REPO="${CONSUL_REPO:-/workspace/legatus-consul-agent}"

log() { printf 'sandbox-bootstrap: %s\n' "$*"; }

mkdir -p "$(dirname "$SBIN")"

if [[ -x "$SBIN/sentinel" ]]; then
    log "sentinel binary cached at $SBIN/sentinel — skipping build"
    "$SBIN/sentinel" --version
    return 0 2>/dev/null || exit 0
fi

if [[ ! -d "$CONSUL_REPO" ]]; then
    log "WARN legatus-consul-agent not mounted at $CONSUL_REPO"
    log "WARN sentinel-legatus needs the sibling repo to build."
    log "WARN check sandbox-compose-override.yml mounts."
    exit 1
fi

if [[ ! -d "$SENTINEL_REPO/crates/sentinel-cli" ]]; then
    log "WARN sentinel repo not mounted at $SENTINEL_REPO"
    exit 1
fi

log "building sentinel-cli (one-time, ~3min)..."
log "  repo: $SENTINEL_REPO"
log "  bin:  $SBIN"
cargo install \
    --path "$SENTINEL_REPO/crates/sentinel-cli" \
    --root "$(dirname "$SBIN")" \
    --locked \
    2>&1 | tail -10

if [[ ! -x "$SBIN/sentinel" ]]; then
    log "ERROR sentinel binary not produced; build failed"
    exit 1
fi

# Put sentinel on PATH. /usr/local/bin needs root; ~/.bashrc works
# unprivileged. We try the global path first (works if container
# entrypoint runs as root) then fall back to the dev user's shell
# rc. The compose override starts the container as the dev user
# so the bashrc path is what actually runs in practice.
if [[ -w /usr/local/bin ]]; then
    ln -sf "$SBIN/sentinel" /usr/local/bin/sentinel
    ln -sf "$SBIN/sentinel-engine" /usr/local/bin/sentinel-engine
    log "symlinked into /usr/local/bin"
else
    # Append once; idempotent on re-runs.
    BASHRC="${HOME}/.bashrc"
    if [[ -f "$BASHRC" ]] && ! grep -qF "$SBIN" "$BASHRC"; then
        printf '\n# sentinel cli (sandbox-bootstrap)\nexport PATH="%s:$PATH"\n' "$SBIN" >> "$BASHRC"
        log "appended PATH export to $BASHRC"
    fi
    # Profile.d works for non-interactive shells too, but only if writable.
    PROFD=/etc/profile.d/sentinel-sandbox.sh
    if [[ -w /etc/profile.d ]] && [[ ! -f "$PROFD" ]]; then
        printf 'export PATH="%s:$PATH"\n' "$SBIN" > "$PROFD"
        chmod a+r "$PROFD" 2>/dev/null || true
        log "wrote $PROFD"
    fi
fi

export PATH="$SBIN:$PATH"
log "ready · $(sentinel --version 2>&1 || echo unknown)"
