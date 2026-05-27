# sentinel sandbox — container settings + hook wiring

Drop-in settings files that wire sentinel sandbox containers
(`legatus-docker-dev`, `sentinel-grind-*`) into the host's sentinel
observability mesh.

## The problem this solves

By default, sentinel sandbox containers boot Claude Code with an
empty `/home/dev/.claude/settings.json` and **no hooks**. Result:
no hook events fire, no JSONL writes, the bridge never sees the
session, and the viz dashboard at `archive.nashgo.org` / the
sentinel-viz frontend can't display the container's work.

Two coordinated mounts unblock visibility:

1. **Settings mount.** Bind-mount `claude-settings-hooked.json` at
   `/home/dev/.claude/settings.json` (the container Claude's
   default settings location, equivalent to `~/.claude/settings.json`
   from inside the container) so Claude fires
   `sentinel hook --event <Name>` for every hook event.
2. **Metrics mount.** Bind-mount the host's
   `~/.claude/sentinel/metrics/` directory at the container's
   `$SENTINEL_CLAUDE_DIR/sentinel/metrics/` so `sentinel hook`
   writes its JSONL to a path the host's `sentinel_bridge.py` is
   already tailing.

`SENTINEL_CLAUDE_DIR` is honored by `sentinel hook` when deciding
WHERE to write JSONL output (see
`crates/sentinel-application/src/paths.rs::claude_dir`). It does
NOT change where Claude Code reads its own config — that always
stays at `$HOME/.claude/`. The two paths are intentionally split
so the container's config lives behind an isolation boundary while
the metrics output flows up to the host bridge.

That's it. After those two mounts the container's session appears in
the bridge SQLite (`~/.agents/scratch/activegraph-bridge/sentinel.db`)
and shows up in the dashboard alongside host sessions.

## Files

| File | Purpose |
|------|---------|
| `claude-settings-hooked.json`   | Hooked profile — default for sandbox containers. Fires sentinel hooks. |
| `claude-settings-hookless.json` | Hookless profile — for `SENTINEL_HOOKS=off`. Identical to the legacy `~/.agents/notes/sentinel-docker-dev/claude-settings.container.json`. |

## Launcher wiring

The legacy launcher (`~/.agents/notes/sentinel-docker-dev/legatus-docker-dev.sh`
and `run-plan.sh`) needs two additions to consume these files. Apply
to the host scratch-dir copies — they are NOT in this repo:

```bash
# 1. Pick which settings file to mount based on SENTINEL_HOOKS:
SENTINEL_HOOKS="${SENTINEL_HOOKS:-on}"
if [[ "$SENTINEL_HOOKS" == "off" ]]; then
  SETTINGS_SRC="/workspace/sentinel/tools/sandbox/claude-settings-hookless.json"
else
  SETTINGS_SRC="/workspace/sentinel/tools/sandbox/claude-settings-hooked.json"
fi

# 2. Add these mounts to the docker run command:
#    -v "${SETTINGS_SRC}:/home/dev/.claude/settings.json:ro"
#    -v "${HOME}/.claude/sentinel/metrics:/workspace/.container-state/claude/sentinel/metrics"
#    (the metrics mount nests under the sentinel-dev-state named
#     volume — docker handles nested binds when -v args are passed
#     in this order.)
```

Both mounts use the sentinel repo path `/workspace/sentinel/tools/sandbox/...`
which is already bind-mounted by the launcher's repo mount, so the
settings file resolves naturally without a host-side copy.

## Env vars

| Var | Default | Effect |
|-----|---------|--------|
| `SENTINEL_HOOKS`     | `on`  | Set to `off` to use the hookless profile. |
| `SENTINEL_WORKSTREAM`| `—`   | Tag forwarded by `sentinel hook` into the bridge — shows in the viz dashboard's per-session label. The launcher sets this from the grind container name. |
| `SENTINEL_CLAUDE_DIR`| `—`   | Set by the launcher (typically `/workspace/.container-state/claude`). The bind-mounts above target this path. |

## Isolation seam (intentional)

Only the **metrics JSONL directory** crosses the host↔container
boundary, not the whole `.claude/` profile. Container-side Claude
keeps its own history, chat sessions, MCP config, and project
state — those stay container-local under `$SENTINEL_CLAUDE_DIR`.
This preserves the "sandbox" property (the container can't read
your real Claude history or write to your host's project memory)
while still letting observability events flow up to the bridge.

## Verifying the wiring

After launching a hooked grind container:

```bash
# Watch the bridge SQLite for new sessions:
sqlite3 ~/.agents/scratch/activegraph-bridge/sentinel.db \
  "SELECT session_id, datetime(last_activity_at,'unixepoch','localtime'), data->>'$.name' \
   FROM nodes WHERE node_type='SentinelSession' \
   ORDER BY last_activity_at DESC LIMIT 5"

# Or hit the viz-api directly:
curl -s http://172.16.100.22:8082/api/graph?limit=20 \
  | jq -r '.nodes[] | select(.type=="SentinelSession" and .last_activity_age_s < 300) \
                     | "\(.data.session_id) \(.session_status)"'
```

A fresh grind should show up with `last_activity_age_s < 60` within
seconds of the first hook firing (SessionStart).
