# M5 — Linear Skill End-to-End Manual Verification Runbook

**Status:** Active manual recipe. Precursor to automated M5.1/M5.2/M5.3 (#42/#43/#44) — those tests need cross-crate test scaffolding that's its own multi-session project. This document gives a future session or a human a tight, reproducible path to verify the linear-skill pipeline end-to-end against real firefly-pro Linear and real GitHub.

**Pre-built from:** M2.3 (linear.toml step enrichment, commit 460146e) + M2.3.0 (skills-mcp ExecutionPlan codegen, commit 76a7aaf in skills-mcp-rust) + the Doppler personal-branch identity chain (linear-mcp #90/#94, slack-mcp #97, notion-mcp #96, loom-mcp #95, gemini-mcp #100, Vulcan SDK #93) + sentinel `cleanup tasks` (#78) and `schema_validator` (#103).

## Prerequisites

### 1. Doppler service token (one-time per machine)

```bash
# Create a service token in the firefly-pro Doppler workspace UI:
#   Workplace Settings → Service Tokens → create one scoped to project
#   "firefly-pro-crm", read-only.

# Export in your shell profile (~/.zshrc / ~/.bashrc / Windows env vars):
export DOPPLER_TOKEN_FIREFLY_PRO=dp.st.xxxxxxxxxxxxxxxxxxxxxxx

# The env var name is fixed by the Vulcan SDK convention:
#   DOPPLER_TOKEN_<ACCOUNT> where <ACCOUNT> is the doppler_account
#   field from ~/.claude/sentinel/projects/firefly-pro.md uppercased
#   with hyphens → underscores. firefly-pro → FIREFLY_PRO.
```

### 2. Personal branch + LINEAR_API_KEY (one-time per developer)

In Doppler UI, in project `firefly-pro-crm`:

1. Under environment `development`, create a branch config named `development_personal_<your_name>` (e.g. `development_personal_gary`). If a personal config already exists for you, use that — Doppler auto-creates them on first edit; the MCP API can't create true Personal Configs with auto-ACL, only branch configs.
2. In your branch, set `LINEAR_API_KEY` to your **personal** Linear API key (Linear → Settings → API → Personal API keys). This is the user-scope key that auths as you, NOT the shared M2M key.
3. Tell linear-mcp which branch to read from. Export in your shell profile:

```bash
export DOPPLER_PERSONAL_BRANCH=development_personal_<your_name>
```

### 3. Restart Claude Code

mcp-router will respawn linear-mcp on next launch (or after rebuild). The new linear-mcp-rust binary reads `LINEAR_API_KEY` via `vulcan::project_secret_with_personal_branch`, honouring the env var override (commit 5dde628). On startup, the server's stderr will log either:

```
INFO  Loaded LINEAR_API_KEY from Doppler for account: gary@fireflypro.com (firefly-pro)
```

or, if anything fails to resolve:

```
DEBUG Doppler LINEAR_API_KEY lookup failed: ...
INFO  Loaded token from keyring for account: ...
```

Verify identity:

```
mcp__linear__viewer
```

Returns your `gary@fireflypro.com` user record. If you see the M2M/bot identity instead, your env vars aren't taking effect — check the export commands actually ran in the shell that spawned Claude Code.

## Schema pre-flight (fast — 5 seconds)

Before any real-API run, validate `config/steps/linear.toml`:

```bash
cd ~/Documents/GitHub/sentinel
cargo test --bin sentinel-engine schema_validator -- --include-ignored
```

Expected output:

```
test schema_validator::tests::production_step_configs_are_clean ... ok
test result: ok. 14 passed; 0 failed; 0 ignored
```

If this fails, fix the dangling forward-refs or suspicious tool names before continuing — every later step assumes the schema is clean.

## Test ticket setup

Create a throwaway Linear test ticket. Don't use a real backlog item — the E2E run mutates state (claim, transition, comment-add), and you want isolation.

```
mcp__linear__create_issue(
    team_id: "3f73ff3b-1298-49e1-a08f-42b29346e828",   // FPCRM team
    title: "M5 E2E test: linear skill verification",
    description: "Throwaway issue for M5 E2E verification. Safe to close.",
    priority: 4,                                         // Low
)
```

Record the returned `issue.identifier` (e.g. `FPCRM-1234`) — every subsequent step references it.

## Stage 1: Backlog → Code Review (M5.1 / #42)

This is the path the linear-skill claim phase + worktree phase + early review phase walks. We're not invoking skills-mcp tools here — we're verifying the workflow they'd dispatch.

### 1.1 Pre-claim guardrail (claim phase, steps -0.1 through -0.4)

```
mcp__linear__get_issue(id: "FPCRM-1234")
```

Assert: `state.type` is `backlog` or `unstarted`, `assignee` is null. If not, the issue isn't actionable — pick another one.

### 1.2 Claim (claim phase, steps 0.1 through 0.4)

```
# 0.1: Look up started state
mcp__linear__get_workflow_states(team_id: "...")
# Find the state where type == "started" — record its id.

# 0.2: Get viewer
mcp__linear__viewer
# Record viewer.id.

# 0.3: Transition + assign
mcp__linear__update_issue(
    id: "<issue uuid from 1.1>",
    state_id: "<started state id from 0.1>",
    assignee_id: "<viewer id from 0.2>",
)

# 0.4: Confirm
mcp__linear__get_issue(id: "FPCRM-1234")
# Assert state.name == "In Progress" and assignee.email == "gary@fireflypro.com"
```

### 1.3 Implement (minimal — no real code change needed for the runbook)

For the runbook, create a trivial change in a worktree just to have a diff:

```bash
cd ~/Documents/GitHub/sentinel
# Use a docs-only worktree so the change is fast and reviewable
git worktree add .claude/worktrees/m5-e2e-runbook-test -b m5-e2e-test main
cd .claude/worktrees/m5-e2e-runbook-test
echo "<!-- M5 E2E test marker FPCRM-1234 -->" >> README.md
git add README.md
git commit -m "test(m5): runbook trivial diff (FPCRM-1234)"
git push -u origin m5-e2e-test
```

### 1.4 Open PR (review phase, Layer 3)

```
mcp__github__pr_create(
    title: "test(m5): runbook trivial diff (FPCRM-1234)",
    body: "Ref FPCRM-1234\n\nM5 E2E test — DO NOT MERGE. Safe to close after the runbook completes.",
    base: "main",
    head: "m5-e2e-test",
    draft: true,
)
```

**Critical**: the body must say `Ref FPCRM-1234`, NOT `Fixes` / `Closes` / `Resolves`. Those trigger Linear's native auto-Done integration which bypasses the QA-Testing transition we want to test in M5.2.

### 1.5 Link PR to issue

```
mcp__linear__link_attachment_github_pr(
    issue_id: "<issue uuid>",
    pr_url: "<pr_url from 1.4>",
)
```

### 1.6 Transition to Code Review

```
mcp__linear__get_workflow_states(team_id: "...")
# Find state.name == "Code Review" — record id.
mcp__linear__update_issue(id: "<issue uuid>", state_id: "<code review state id>")
mcp__linear__get_issue(id: "<issue uuid>")
# Assert state.name == "Code Review", assignee unchanged (still you).
```

### 1.7 Verification checklist for Stage 1

- [ ] Issue transitioned `Backlog → In Progress → Code Review`
- [ ] Issue assignee is `gary@fireflypro.com` (your personal identity, not the bot)
- [ ] PR body literally contains `Ref FPCRM-1234`
- [ ] PR is linked to the issue (appears in issue's Attachments)
- [ ] No `Fixes`/`Closes`/`Resolves` in PR title or body

**If Stage 1 passes, the linear-mcp Doppler identity wiring is working AND the skill's claim/review path semantics are correct in real Linear.**

## Stage 2: Code Review → QA Testing → Completed (M5.2 / #43)

The merge transition is automated via `.github/workflows/linear-on-merge.yml` (per CLAUDE.md). Test that by merging the PR.

### 2.1 Merge

```bash
gh pr merge <pr_number> --squash  # or whatever the project's merge strategy is
```

### 2.2 Wait for linear-on-merge workflow

```
mcp__github__workflow_list_runs(repo: "firefly-pro-crm", workflow: "linear-on-merge.yml")
# Wait for the most recent run to complete (status: completed, conclusion: success).
# Should take ~30-60 seconds.
```

### 2.3 Verify auto-transition to QA Testing

```
mcp__linear__get_issue(id: "<issue uuid>")
# Assert state.name == "QA Testing"
# Assert assignee.email == "pedro.bordignon@fireflypro.com" (the QA tester from
# the firefly-pro project config qa_email field)
```

### 2.4 QA pass (manual — emulating Pedro)

If you want to walk the full Completed path, transition the issue manually:

```
mcp__linear__get_workflow_states(team_id: "...")
# Find state.type == "completed" — record id.
mcp__linear__update_issue(id: "<issue uuid>", state_id: "<completed state id>")
mcp__linear__get_issue(id: "<issue uuid>")
# Assert state.name == "Completed"
```

### 2.5 Verification checklist for Stage 2

- [ ] PR merged successfully
- [ ] `linear-on-merge.yml` workflow ran and succeeded
- [ ] Issue auto-transitioned `Code Review → QA Testing`
- [ ] Issue auto-reassigned `gary → pedro.bordignon`
- [ ] Manual transition `QA Testing → Completed` works

**If Stage 2 passes, the merge automation + QA handoff semantics are correct.**

## Stage 3: Race conditions + batch ops (M5.3 / #44)

Stress test only — run after Stage 1 + 2 pass cleanly. Tests that mcp__linear__batch_create_issues + concurrent updates don't corrupt state.

### 3.1 Batch create

```
mcp__linear__batch_create_issues(
    team_id: "...",
    issues: [
        { title: "M5.3 stress 1", priority: 4 },
        { title: "M5.3 stress 2", priority: 4 },
        ...10 total
    ],
)
```

Record all 10 returned issue IDs.

### 3.2 Concurrent claim

In rapid succession (script this — don't manually click), for each issue:

```
mcp__linear__update_issue(
    id: "<each issue uuid>",
    state_id: "<in progress>",
    assignee_id: "<viewer id>",
)
```

### 3.3 Verify

```
for each id: mcp__linear__get_issue(id)
# Assert all 10 are in In Progress, assigned to you, no duplicates, no errors.
```

### 3.4 Cleanup

Delete the test issues (or cancel them — Linear preserves the audit trail either way):

```
for each id: mcp__linear__delete_issue(id)
# Plus the original FPCRM-1234 from Stage 1/2.
```

## Stage 4: StepProof chain verification

Independent of Stages 1-3 — verifies that sentinel's proof chain captures the same workflow.

For each transition in Stages 1-2, you could optionally call `mcp__sentinel__submit_step_complete` to seal a step in the chain. Today this is opt-in (no hooks auto-fire during a Linear-MCP-driven workflow). Promoting it to automatic on every Linear state change is the natural follow-up — file as a new task if Stage 4 is something the team wants automated.

Skipping Stage 4 doesn't invalidate Stages 1-3. Manual chain assembly:

```
# After each successful step:
mcp__sentinel__submit_step_complete(
    skill: "linear",
    phase: "claim",
    step_id: "0.3",
    evidence: { issue_id: "FPCRM-1234", transitioned_to: "In Progress", assignee: "<viewer_id>" },
    judge_verdict: { sufficient: true, confidence: 1.0 },
    session_id: "<your active session id>",
)

# At the end, inspect the full chain:
mcp__sentinel__get_step_chain(session_id: "...")
# Assert: head_hash is not the zero hash, every StepProof's previous_hash
# matches the prior entry's combined_hash, step IDs appear in workflow order.
```

## What this runbook is NOT

- **Not a substitute for M5.1/M5.2/M5.3 automated tests.** Those need cross-crate dependency wiring (sentinel-application or a dedicated test harness binary that can call both linear-mcp and sentinel-mcp via MCP protocol, OR use linear-application directly with sentinel-application's submit_step_complete). Filing as #41 (parent) with the three children remains the real-world path — this runbook is the manual fallback until that scaffolding ships.
- **Not isolated from prod Linear.** This mutates real firefly-pro Linear state. The test ticket isolates the blast radius but you ARE making real API calls. Use a throwaway issue and clean up.
- **Not a CI artifact.** Don't try to run this in CI — it needs interactive Doppler/Linear auth and a real PR/merge cycle.

## Quick sanity checklist (5 minutes)

If you only want to verify the identity chain is working (the part most likely to break across sessions), skip to:

1. `mcp__linear__viewer` → returns gary@fireflypro.com? ✓
2. `cargo test --bin sentinel-engine schema_validator -- --include-ignored` → all green? ✓
3. `mcp__linear__create_issue` → returns a new FPCRM ticket assigned to nobody, in Backlog? ✓ (then delete it)

If those three pass, the foundation is solid and any failure in Stages 1-3 is a workflow issue, not an auth/identity issue.

## Related task IDs

- #41 — M5 parent epic
- #42 — M5.1 automated Backlog → Code Review test (deferred to fresh session)
- #43 — M5.2 automated Code Review → Completed test (deferred)
- #44 — M5.3 automated race/batch stress test (deferred)
- #103 — M5.0 schema validator (shipped 2026-05-14)
- #104 — this runbook (shipped 2026-05-14)
- #17 — M2.3 linear skill step-gating (shipped)
- #90/#94 — linear-mcp Doppler identity wiring (shipped)
