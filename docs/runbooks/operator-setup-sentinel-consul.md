# Runbook — Standing up sentinel ↔ consul from scratch

This is the operator-facing path from zero to a running sentinel daemon
talking to a consulate. Follow it once per machine; subsequent boots
just need `sentinel daemon` (the daemon reads its config from the
files written here).

Audience: operator deploying the catastrophic-action authorization
system on their own workstation. Not for end users of Claude Code —
they only need Claude Code running; the daemon stays out of their way.

## 1. Prerequisites

- macOS or Linux. Rust toolchain 1.83+.
- Both repos cloned side-by-side:
  ```
  ~/Documents/GitHub/sentinel
  ~/Documents/GitHub/legatus-consul-agent
  ```
  (path-deps in `Cargo.toml` resolve via `../legatus-consul-agent/`)
- Optional but recommended: `jq` (`brew install jq`).

## 2. Build the two binaries

```bash
cd ~/Documents/GitHub/sentinel
cargo build --release -p sentinel

cd ~/Documents/GitHub/legatus-consul-agent
cargo build --release -p consulate
# (Optional: also build consul-app if you want the operator brain.)
```

## 3. Generate a shared bootstrap secret

The consulate and every legatus that talks to it share a 32-byte
secret. Generate one and **keep it private** — anyone with this
secret can pose as a legatus.

```bash
sentinel legatus init
# (or, until that subcommand ships:)
openssl rand -hex 32
```

Save the output to **both** of:
- `~/.config/consulate/bootstrap-secret` (consulate-side)
- `~/.config/sentinel/legatus-bootstrap-secret` (sentinel-side)

Make both files `0600`:

```bash
chmod 0600 ~/.config/consulate/bootstrap-secret \
            ~/.config/sentinel/legatus-bootstrap-secret
```

## 4. Start the consulate

In one terminal:

```bash
consulate \
  --bind 127.0.0.1:9000 \
  --insecure-localhost-only \
  --bootstrap-secret "$(cat ~/.config/consulate/bootstrap-secret)" \
  --db-url "sqlite:$HOME/.local/share/consulate/state.db"
```

The `--insecure-localhost-only` flag is required while consulate
ships `ws://` only. Production TLS (`wss://`) is on the roadmap
(see `Tier 3.8` in the connection-reliability tasks).

Verify it's up:

```bash
lsof -nP -iTCP:9000 -sTCP:LISTEN
```

## 5. Start the sentinel daemon

In another terminal (the daemon stays running; one per operator
machine):

```bash
export CONSULATE_BOOTSTRAP_SECRET="$(cat ~/.config/sentinel/legatus-bootstrap-secret)"

sentinel daemon \
  --port 3001 \
  --legatus-consulate-url ws://127.0.0.1:9000 \
  --legatus-suggested-name "$(hostname -s)" \
  --legatus-heartbeat-secs 20
```

The daemon writes its per-instance bearer token to
`~/.claude/sentinel/daemon-token` (format: `port:token`). Hook
subprocesses read this file to authenticate; the file is mode `0600`.

## 6. Verify the connection

The daemon's `/legatus/health` endpoint reports the wrapper's
current connection state. Bearer-authed:

```bash
TOKEN_LINE=$(cat ~/.claude/sentinel/daemon-token)
DAEMON_PORT="${TOKEN_LINE%%:*}"
DAEMON_TOKEN="${TOKEN_LINE#*:}"

curl -s -H "Authorization: Bearer ${DAEMON_TOKEN}" \
  "http://127.0.0.1:${DAEMON_PORT}/legatus/health" | jq .
```

You want to see:

```json
{ "status": "connected" }
```

Other expected states:
- `connecting` — first attempt in flight; brief.
- `reconnecting` — previously connected, currently in backoff
  between retries.
- `disconnected` — wrapper exited (cancel, fatal `VersionMismatch`).

## 7. Optional — bind to a specific operator identity

If you want sentinel-emitted escalations to route to your specific
operator (multi-operator deployments), bind a `--legatus-operator-id`:

```bash
sentinel daemon \
  --port 3001 \
  --legatus-consulate-url ws://127.0.0.1:9000 \
  --legatus-bootstrap-secret "$(cat ~/.config/sentinel/legatus-bootstrap-secret)" \
  --legatus-operator-id 4a0c8e7d-9b12-4e5f-8a3d-1f6b9c2d0e4a \
  --legatus-suggested-name "$(hostname -s)"
```

Without it, sessions register as `OperatorId::ROOT` — the v0.1
single-operator scaffold. Per-operator voice routing on the consul
side requires this binding.

## 8. Run the smoke test to confirm everything is wired

```bash
cd ~/Documents/GitHub/sentinel
SENTINEL_BIN=$(pwd)/target/release/sentinel \
CONSULATE_BIN=~/Documents/GitHub/legatus-consul-agent/target/release/consulate \
  bash scripts/smoke-sentinel-consul-roundtrip.sh
```

You want to see all five `==>` steps pass, ending with:

```
PASS: sentinel <-> consul roundtrip + reconnect verified
```

The smoke test uses ephemeral ports + an isolated `$HOME` so it
doesn't interfere with your running daemon.

## 9. Troubleshooting

**`/legatus/health` returns `connecting` or `reconnecting` forever**
- Check the consulate is actually listening on the URL you passed:
  `lsof -nP -iTCP:9000 -sTCP:LISTEN`
- Check the bootstrap secret matches on both sides (the consulate
  rejects with `Handshake` errors visible in its log).
- Check the daemon log (default: stderr) for `legatus connecting`
  followed by `failed; reconnecting after backoff` — the reason
  field tells you exactly what went wrong (transport, handshake,
  protocol version).

**`/legatus/health` returns 401**
- Wrong bearer token. Re-read `~/.claude/sentinel/daemon-token`;
  the file is rewritten on every daemon start.

**Daemon dies on startup with "missing field operator_id"**
- The consulate and sentinel-legatus protocol-version pins must
  match. Rebuild both repos from the same `main` commit.

**Catastrophic command is blocked but consulate never sees it**
- Check the daemon log for `escalation send: ...` warnings. The
  daemon enqueues escalations on the legatus handle; the WS loop
  drains them. If the WS is down, the persistent outbox
  (`~/.claude/sentinel/state/legatus-escalations.jsonl`) holds
  them until reconnect.

**Hook subprocess shows `legatus_client: cannot build reqwest client`**
- Almost always a permissions issue on `~/.claude/sentinel/daemon-token`.
  Check it's mode `0600` and owned by you.

## 10. Tearing it down

```bash
# Sentinel daemon — Ctrl+C in its terminal, or:
pkill -f "sentinel daemon"

# Consulate — Ctrl+C, or:
pkill -f "consulate"

# Clean up the token file so stale tokens don't confuse anything:
rm -f ~/.claude/sentinel/daemon-token
```

For a production-style setup with launchd / systemd unit files,
see `Tier 2.4` of the connection-reliability follow-ups.
