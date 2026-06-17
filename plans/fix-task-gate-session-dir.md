# Fix: task-decomposition-gate blocks all mutating tools (session-dir + stderr-redir bugs)

## Problem
The `task-decomposition-gate` (and sibling task hooks) read the session task list
from `~/.claude/tasks/{session_id}/`, joining the **raw** `session_id` (a full UUID,
e.g. `e2ea5630-3c79-409c-9ca4-423975a5a5fb`). But Claude Code's native task store
writes to `~/.claude/tasks/session-{first8}/` (e.g. `session-e2ea5630/`). The dir
never matches -> `has_live_task_list` returns `Some(false)` -> the gate fails CLOSED
and blocks **every** mutating tool, even with a valid live task list. There is no
`.highwatermark` written for native-store sessions, so that fallback can't save it.

Secondary bug: `is_mutating_bash` returns `true` for any command containing `>`,
which flags benign **stderr redirections** (`2>&1`, `2>/dev/null`, `N>&M`) as
mutating writes. Read-only commands like `ls -la 2>&1` get blocked.

## Fix (in `task_decomposition_gate.rs`)
1. Add `resolve_session_dir(fs, home, session_id)`: return `~/.claude/tasks/{session_id}/`
   if it is a dir; else try the harness naming `~/.claude/tasks/session-{first8}/`
   (first 8 chars of the id, prefixed `session-`); else return the literal path
   (so the `is_dir` check still yields `Some(false)` for genuinely fresh sessions).
2. Use it in `has_live_task_list`.
3. Harden `is_mutating_bash`: strip stderr redirections before the `>` check, so
   only real stdout/file writes (`>`, `>>`) count as mutating.
4. Add unit tests for both: `session-{first8}` resolution and stderr-redir exemption.

## Verify
- `cargo test -p sentinel-application task_decomposition` green
- `cargo build --release -p sentinel-cli` (sentinel-engine)
- `sentinel stage`

## Scope
Worktree `fix/task-gate-session-dir`. Gate-only change; sibling hooks share the
same naive join (latent same bug) - fast follow.
