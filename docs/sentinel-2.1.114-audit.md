# Sentinel 2.1.114 Audit Findings

Audit of sentinel hook correctness against Claude Code 2.1.114 source, extracted via the decompiler pipeline at `claude-code-system/decompiler`.

## Method

- Downloaded `@anthropic-ai/claude-code@2.1.114` thin wrapper and `@anthropic-ai/claude-code-win32-x64@2.1.114` native binary (73MB packed / 235MB unpacked)
- Ran `decompile auto --bun-binary claude.exe` — recovered **29,422 name mappings** (2,304 first-party Anthropic), produced 13MB decompiled JS + 17K-line `index.d.ts`
- Diffed `package/sdk-tools.d.ts` 2.1.88 → 2.1.114 (only 7 hunks, 21 line difference)
- Ran 6 parallel research agents against the decompiled source for targeted findings

## Findings

### 1. `EnterPlanMode` IS real — earlier correction was wrong

Confirmed from decompiled 2.1.114 binary (`handler r7H` at line 1666): `EnterPlanMode` is a live, model-callable tool. Its `call()` handler rejects inside agent contexts (`if ($.agentId) throw Error("EnterPlanMode tool cannot be used in agent contexts")`) but succeeds in the main session.

It is **omitted from the public `sdk-tools.d.ts`** in BOTH 2.1.88 and 2.1.114 — the ToolInputSchemas union never had it. This is the source of the earlier confusion: I trusted the declared types and missed a schema-hidden real tool.

**Impact:** the earlier change that removed `EnterPlanMode` from `phase_exempt_tools` was a regression — it would gate legitimate plan-mode entry. This commit re-adds it with an explanatory comment citing the binary evidence, flips the regression test assertion, and adds a new PostToolUse handler that marks plan-approved on `EnterPlanMode` (in addition to `ExitPlanMode`).

### 2. New plan-mode entry paths in 2.1.114

In addition to the 2.1.88 entry paths (Shift+Tab, env var, Agent `mode:"plan"`, agent YAML `permissionMode:"plan"`), 2.1.114 adds:

- **`EnterPlanMode` tool** — model-callable, main-session only (see #1)
- **CLI flag `--permission-mode plan`** on both `claude` and `claude bridge` — validated against `PERMISSION_MODES = ["acceptEdits","auto","bypassPermissions","default","dontAsk","plan"]`
- **SDK `control_request` RPC** `{subtype:"set_permission_mode", mode:"plan", ultraplan?:bool}` — used by the streaming/bridge channel and by the `/ultraplan` slash command

### 3. New permission mode: `auto`

`PERMISSION_MODES` in 2.1.114: `["acceptEdits","auto","bypassPermissions","default","dontAsk","plan"]`. 2.1.88 had no `auto`. The `Agent` tool's `mode` field now also accepts `"auto"`.

Sentinel's code doesn't enumerate permission modes, so no code change needed — but worth tracking.

### 4. `EnterWorktree` schema added `path` field

Per `sdk-tools.d.ts` diff:

```ts
// 2.1.88
interface EnterWorktreeInput { name?: string }
// 2.1.114
interface EnterWorktreeInput {
  name?: string  // mutually exclusive with path
  path?: string  // enter existing worktree instead of creating
}
```

Sentinel's `worktree_reminder` hook doesn't inspect tool inputs, so no impact.

### 5. Agent stats field added

`AgentOutput` (completed variant) gained `toolStats?: { readCount, searchCount, bashCount, editFileCount, linesAdded, linesRemoved, otherToolCount }`.

No sentinel impact.

### 6. `BashOutput` gained `staleReadFileStateHint`

New hint field set when write commands detect a readFileState mtime bump. No sentinel impact.

### 7. `FileEditOutput.originalFile` is now nullable

Was `string`, now `string | null`. No sentinel impact.

### 8. Hook events — no drift

All 27 hook events in 2.1.88 are present in 2.1.114's `_V` array. Full set: `PreToolUse, PostToolUse, PostToolUseFailure, Notification, UserPromptSubmit, SessionStart, SessionEnd, Stop, StopFailure, SubagentStart, SubagentStop, PreCompact, PostCompact, PermissionRequest, PermissionDenied, Setup, TeammateIdle, TaskCreated, TaskCompleted, Elicitation, ElicitationResult, ConfigChange, WorktreeCreate, WorktreeRemove, InstructionsLoaded, CwdChanged, FileChanged`.

Sentinel still does not dispatch: `Notification, PermissionRequest, Elicitation, ElicitationResult, ConfigChange, InstructionsLoaded, FileChanged` — highest-leverage addition remains `ConfigChange` (for authoritative plan-mode-transition detection, which would supersede both `EnterPlanMode` PostToolUse and the env-var check).

### 9. `TodoWrite` / `AskUserQuestion` schemas unchanged

Byte-identical between 2.1.88 and 2.1.114. Sentinel's detection code in `hook_cmd.rs` remains correct.

### 10. `AgentInput` schema — only `mode` gained `"auto"`

Otherwise byte-identical. No impact on `phase_gate` / `orchestration_nudge`.

## Corrections applied in this commit

- `workflow.rs::should_block` exempt list: re-added `EnterPlanMode` with citation to binary handler
- `phase_gate.rs::process` exempt list: same
- `workflow.rs::test_phase_exempt_tools_not_blocked` regression test: flipped assertion to require `EnterPlanMode` is exempt (not gated)
- `hook_cmd.rs` PostToolUse dispatcher: `EnterPlanMode` now marks plan-approved (alongside `ExitPlanMode`)
- `session_init.rs` CLAUDE.md template: Plan Mode workflow step 1 now lists `EnterPlanMode` as an entry path, along with all other 2.1.114 entry mechanisms including `--permission-mode plan` CLI flag
