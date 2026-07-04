# MCP Registration Guardian

Sentinel's control loop over the `mcpServers` block in `~/.claude.json` — the
live registry Claude Code reads at session start. That block has been lost
before (the Jun 17-18 2026 global-config corruption/rebuild) and the loss was
invisible because the scanner coerced every failure mode to a `0` count.

## Ownership split (architecture decision, pinned)

| Component | Role |
|-----------|------|
| **sentinel** | OWNS detect + snapshot + heal + alert (`mcp_guardian` module, run from the `session_init` SessionStart hook) |
| **marketplace repo** | DECLARATIVE ONLY — `marketplace.json` `mcp[]` entries + a `retired` tombstone array; never healed into, never carries literal secrets |
| **claude-code-handler** | ZERO-touch — no handler edits of any kind |

## Registry contract v1 (what the marketplace declares)

Each `marketplace.json` `mcp[]` entry is full-fidelity:

```json
{
  "mcp": [
    {
      "name": "linear",
      "command": "mcp-supervisor",
      "args": ["mcp-router", "--single", "C:/.../linear-mcp.exe", "--watch", "C:/.../linear-mcp.exe"],
      "transport": "stdio",
      "env": {
        "RUST_LOG": "info",
        "LINEAR_API_KEY": "$doppler:firefly/dev/LINEAR_API_KEY"
      }
    }
  ],
  "retired": ["agents", "skills"]
}
```

- `$doppler:<project>/<config>/<SECRET>` env refs are resolved by sentinel at
  heal time (`doppler-rs secrets get <SECRET> --plain -p <project> -c <config>`,
  falling back to the `doppler` binary). The marketplace repo may be public —
  it never carries a literal secret. The healed `~/.claude.json` may carry
  resolved values (it is local-only).
- A ref that fails to resolve is **omitted with a warning** and the server
  entry is still healed — a degraded registration beats an absent one.
- Non-secret env (`RUST_LOG`, ports, …) stays literal.
- `retired` names are tombstones: the guardian never heals them back, and
  actively removes them on merge. `agents` and `skills` (merged into the
  unified `sentinel-mcp`, marketplace commit fbd2f90) are additionally
  hard-coded in `mcp_guardian::RETIRED_BUILTIN` so even a pre-merge snapshot
  cannot resurrect them.
- Machine-specific absolute paths are accepted in v1 (personal marketplace).
  `$HOME` templating is a known follow-up.

## The control loop (every SessionStart)

1. **Detect** — `scanner::mcp_registry_state(home)` classifies `~/.claude.json`:
   - `Missing` — file absent, or no `mcpServers` key
   - `Unreadable` — file present but unreadable / invalid JSON
   - `Tampered` — a `_mcpServers_disabled` stash marker exists, or `mcpServers`
     is not a JSON object
   - `Count(n)` — structurally valid registry with `n` entries

   `scanner::count_mcp_servers()` remains as a display-only wrapper (all
   failures collapse to 0); anything that needs truth uses the enum.

2. **Snapshot** — when the state is `Count(n)` with `n >= 10` (sane floor), a
   dated known-good copy of the `mcpServers` object is written to
   `~/.claude/sentinel/state/mcp-registry/registry-YYYYMMDD.json` — at most one
   per day, newest 14 kept, older pruned.

3. **Tripwire** — when the state is compromised (`Missing`/`Unreadable`/
   `Tampered`) or `Count(0)` while `*-mcp-rust` repos exist on disk:
   - a loud `[MCP REGISTRATION MISSING]` line in the startup banner
   - an `mcp_registration_missing` channel event (session-scoped, picked up by
     the sentinel-mcp push channel)

4. **Heal** — on `Missing`, `Tampered`, or empty-while-repos-exist:
   - build the desired registry from the marketplace clone's declaration
     (contract above); fall back to the **newest state snapshot** when the
     declaration is absent or invalid
   - merge into **both** the global `~/.claude.json` and the session config
     `$CLAUDE_CONFIG_DIR/.claude.json` (when set and distinct): existing
     non-retired entries kept, declared entries inserted/overwritten, retired
     names removed, the `_mcpServers_disabled` marker dropped, **every other
     key preserved**
   - writes are atomic (tmp + rename) under an fs2 exclusive lock on a
     `<file>.sentinel-lock` sidecar (the sidecar, not the target, because
     Windows cannot rename over a locked-open file)
   - the standing `/reload-plugins` `initialUserMessage` autoheal in
     `session_init` then reconnects the healed servers in the same session
   - `Unreadable` is deliberately **alert-only**: merging into a file we cannot
     parse would clobber unknown user state — the banner points at the snapshot
     directory for manual restore instead

## Key code

| Piece | Location |
|-------|----------|
| Registry-state classifier | `crates/sentinel-application/src/scanner.rs` (`McpRegistryState`, `mcp_registry_state`) |
| Guardian (snapshot/heal) | `crates/sentinel-application/src/mcp_guardian.rs` |
| Wiring + banner + channel event | `crates/sentinel-application/src/hooks/session_init.rs` (step 4.5, `build_startup_context`) |
