# Plan: Tool Usage Gate — Autopilot Bypass + Stale EnterPlanMode Cleanup

## Problem

tool_usage_gate.rs blocks Edit/Write when no plan-approved marker exists for the
session. The marker is written by the PostToolUse dispatcher when EnterPlanMode
or ExitPlanMode fires. In this session those tools are not deferred-tool-
registered (ToolSearch returns "No matching deferred tools"), and
CLAUDE_CODE_PLAN_MODE_REQUIRED is not set in settings.json. That creates a hard
deadlock: the gate demands a state transition the model cannot trigger.

Also, tool_usage_gate.rs still carries stale comments + a test assertion
claiming "There is no EnterPlanMode tool — must not reference fake tool".
That contradicts the 2.1.114 audit (commit 8f1032e) which confirmed
EnterPlanMode IS a real model-callable tool (binary handler r7H), just
omitted from sdk-tools.d.ts.

## Approach

Two fixes in crates/sentinel-application/src/hooks/tool_usage_gate.rs:

### Fix 1 — Autopilot bypass for plan-mode check

Mirror the pr_merge_gate.rs pattern: when SENTINEL_AUTOPILOT=1 is set, skip
the plan-mode precondition. The user has opted into autonomous execution,
and the other three checks (sequential thinking, task created, task active)
still enforce structure.

### Fix 2 — Stale EnterPlanMode references

- Update module doc + check-3 comment to list all 2.1.114 entry paths
  including EnterPlanMode (real tool, handler r7H, rejects in agent contexts).
- Update the deny message to mention EnterPlanMode as the primary entry path.
- Flip the regression test assertion !reason.contains("EnterPlanMode") to
  reason.contains("EnterPlanMode").
- Update the test's inline comment.

## Verification

1. cargo check -p sentinel-application
2. cargo test -p sentinel-application tool_usage_gate
3. Add a new test test_autopilot_bypasses_plan_gate that sets
   SENTINEL_AUTOPILOT=1, supplies only SEQUENTIAL + TASK + TASK_ACTIVE markers
   (no PLAN marker), and asserts the hook allows.

## Out of scope

- Registering EnterPlanMode/ExitPlanMode as deferred tools (harness-side
  concern, tracked in task #14).
- Restructuring the gate's marker-file storage.
