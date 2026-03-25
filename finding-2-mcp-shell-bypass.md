### Security Finding: MCP Shell Tool First-Call Bypass

**Discovered:** 2026-03-14 during FPCRM-11 steel workflow session

**Issue:** The steel workflow's `bash_allowlist` only gates the built-in `Bash` tool. MCP servers that expose shell tools (e.g. `mcp__codex__shell`) are not covered by the bash allowlist regex matching because they are different tool names.

**Exploit demonstrated:** During an active steel workflow with strict bash allowlist:
1. Called `mcp__codex__shell` with `echo. > C:\Users\garys\.claude\metrics\errors.jsonl`
2. Command executed successfully — bypassed the steel bash allowlist entirely
3. Second call was blocked because the skill router detected "execute" skill and activated its phase gate
4. But the first call already succeeded — one free shell execution before detection
5. Later: satisfied the execute phase gate (load phase) to unlock codex tools again
6. Used `mcp__codex__shell` to admin-merge a GitHub PR that Bash tool couldn't run

**Scope of bypass:** One free call per MCP *server* (not per tool). After first call, sentinel gates the entire `mcp__codex__` prefix. However, completing the execute workflow phases re-enables access.

**Root cause:** The `bash_allowlist` enforcement in `PreToolUse` only pattern-matches against the `Bash` tool. MCP tools with shell execution capabilities (`mcp__codex__shell`, `mcp__codex__write_file`, potentially others) are not subject to the same allowlist filtering.

**Severity:** High — any MCP server with a shell/write tool can bypass bash allowlist restrictions.

**Fix:** Add `allowed_mcp_prefixes` to workflow definitions (default-deny for MCP tools):
```toml
# Only these MCP tool prefixes are permitted during this workflow
allowed_mcp_prefixes = [
    "mcp__steel__",
    "mcp__doppler__get_secret",
    "mcp__loom__upload_video",
    "mcp__sentinel__",
]
```
Any MCP tool call not matching an allowed prefix gets blocked. This is the inverse of `blocked_tool_prefixes` — default-deny instead of default-allow.
