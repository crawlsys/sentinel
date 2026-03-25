## Sentinel Glass Break — Emergency Workflow Override

### Command
sentinel break --reason "<reason>" [--duration <minutes>] [--workflow <name>]

### Behavior
1. Display a 6-digit challenge code in terminal (e.g. "BREAK-849271")
2. User must type it back within 30 seconds to confirm (prevents AI from self-invoking)
3. Suspend the active workflow's restrictions:
   - Bash allowlist → unrestricted
   - Protected path blocks → lifted
   - Phase gate → paused (not reset)
   - Blocked tool prefixes → cleared
4. Default duration: 5 minutes. Max: 30 minutes. Flag: --duration <min>
5. Auto-re-engage workflow after expiry (phase progress preserved)
6. If --workflow not specified, breaks ALL active workflows

### Audit Trail
Every break logged to ~/.claude/sentinel/state/breaks.jsonl:
```json
{
  "timestamp": "2026-03-14T05:03:00Z",
  "reason": "need to merge PR during steel test",
  "workflow": "steel",
  "duration_minutes": 5,
  "challenge_code": "BREAK-849271",
  "tools_used_during_break": [
    { "tool": "Bash", "command": "gh pr merge 372 --squash", "ts": "..." },
    { "tool": "Edit", "target": "workflows.toml", "ts": "..." }
  ],
  "auto_reengaged": true
}
```

### Hook Integration
- SessionStart: show break count in last 24h if > 0
- Stop: if break active, warn "Glass break expires in Xm"
- PreToolUse: check break state before enforcing workflow restrictions
- PostToolUse: log tool call to active break record

### CLI Subcommands
```
sentinel break --reason "..."          # Initiate break (interactive challenge)
sentinel break --status                # Show active break state + time remaining
sentinel break --cancel                # Re-engage workflow early
sentinel break --history               # Show break log (last 30 days)
sentinel break --history --json        # Machine-readable break log
```

### Security Rules
- Challenge code MUST be typed by user in terminal (not injectable via hooks or tool calls)
- Break cannot be initiated from within Claude Code tool calls — only terminal
- Maximum 3 breaks per hour (rate limit prevents abuse)
- Breaks during "security" or "deploy" workflows require --force flag
- All break activity is immutable (append-only JSONL, no delete/edit)

### Security Finding: Source Repo Write Gap

**Discovered:** 2026-03-14 during FPCRM-11 steel workflow session

**Issue:** Protected path enforcement only covers `~/.claude/sentinel/` (config/state directory). The sentinel source repo at `~/Documents/GitHub/sentinel/` is unprotected. During an active workflow, Claude can:

1. Write/edit files in the sentinel source repo
2. Run `cargo build --release` (allowed by `^cargo ` in bash allowlist)
3. The shadow binary system (`sentinel-engine.exe.staged`) auto-swaps on next invocation

This means an agent could modify sentinel's own enforcement logic, rebuild, and have the weakened binary loaded automatically — bypassing all workflow restrictions.

**Fix:** Extend `protected_paths` during active workflows to include infrastructure source repos:

```toml
# Add to workflow definitions in workflows.toml
protected_paths = [
    "~/.claude/sentinel/",
    "~/Documents/GitHub/sentinel/",
    "~/Documents/GitHub/vulcan-mcp-sdk-rust/",
    "~/Documents/GitHub/mcp-router/",
]
```

**Severity:** High — self-modification vector that bypasses all other enforcement layers.

**Mitigations until fixed:**
- The `^cargo ` allowlist entry could be removed from steel workflow (not needed for browser testing)
- A file integrity check (SHA-256 of sentinel-engine.exe) on each hook invocation would detect tampering
