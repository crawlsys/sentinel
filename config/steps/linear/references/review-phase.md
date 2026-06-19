# Linear skill — review phase (steps 3.L0.1 through 3.L5.2)

Reference for the 22 steps in `config/steps/linear.toml` under `[[phases]] id = "review"`. The phase is a 6-layer pipeline that takes a finished implementation through tests → self-review → Codex review → PR creation → CI + CodeRabbit → merge.

## Why six layers, not one

Each layer catches a different class of defect, and the layers are *ordered by cost*: cheapest checks fire first, expensive ones (Codex, CodeRabbit, browser UI tests via CDP for local and Browserbase for preview) only run on code that already passed the cheap ones. The point isn't redundancy; it's that **a failure at a later layer is rare enough to be alarming and cheap enough to act on**.

| Layer | Catches | Typical failure rate at this stage |
|-------|---------|------------------------------------|
| L0 — Tests | Regressions, broken builds | High on first iteration, drops fast |
| L1 — Code review agent | Style, obvious bugs, unused code | Medium |
| L2 — Codex pre-push | Subtle correctness, security, perf | Low |
| L3 — PR create | (No defects — just the SCM artifact) | N/A |
| L4 — CI + CodeRabbit | Integration issues, lint drift, large-codebase context | Low |
| L5 — Final gate | (Verification only — no new defects found here) | ~0 |

If L5 is finding new defects regularly, the phase is broken — earlier layers aren't doing their job.

## Layer 0: Tests (steps 3.L0.1 through 3.L0.5)

Run the full test suite. Compare against the **baseline** captured in Phase 2 (worktree, step 2.4 + 2.5). Any test that passed at baseline but fails now is a **regression** and is a blocker. New failing tests are allowed only if they're the new tests added by this change (step 3.L0.4).

### Baseline vs current

The whole point of capturing a baseline before implementation is to distinguish "this test was already broken" from "this PR broke it." Without baseline, every flaky test on main blocks every PR forever. With baseline, only *new* failures block.

### Writing new tests (3.L0.4)

If the implementation in Phase 2.7 adds new behavior, this step adds tests for it. Not required (the agent might be fixing a bug whose existing test now passes), but encouraged. Test files for new behavior go in the same crate as the behavior, mirroring existing layout — `src/foo.rs` gets tests in `#[cfg(test)] mod tests` at the bottom; integration tests go in `tests/foo_integration.rs`.

## Layer 1: Code review agent (steps 3.L1.1 through 3.L1.3)

`mcp__agents__code_reviewer` (or `Agent(subagent_type: "code-reviewer")`) runs over the diff. Multi-lens by default (security, performance, tests, style, accessibility). Each lens returns a `Verdict { lens, verdict, evidence, severity }` (M3.3, commit d0a14c7).

Fix everything the lens flags as `severity: critical` or `high` before continuing. `medium` and `low` are *advisory* — fix when cheap, acknowledge with reasoning when not. The pattern: every dismissed finding gets one line of reasoning so the audit trail is legible.

### When to escalate to a human

If the agent's reviewer flags a finding the developer disagrees with strongly enough to want to dismiss without fixing: write the dismissal reasoning, but also flag it for the next layer's human review (typically CodeRabbit at L4). Don't just drop it.

## Layer 2: Codex pre-push (steps 3.L2.1 through 3.L2.5)

`mcp__codex__codex` reviews the diff with GPT-5.3 (or whatever the current best closed-frontier model is). This is **cross-vendor verification** — the lens agent in L1 was a Claude-family model; Codex is an OpenAI-family model. Disagreement between them is meaningful signal (see #69 / Stage B: multi-judge verdict types with disagreement detection).

### Why pre-push, not post-push

CI runs after push, but Codex review costs more than CI in agent dollars and human time spent reviewing its output. Run it BEFORE pushing so you don't push code that you'll have to revert. The push itself is the irreversible commitment — once your team sees the PR, they may start reviewing it.

### Critical and high are blockers

If Codex flags `severity: critical` or `high`, fix them in the same worktree before pushing. The gate at 3.L2.5 fails if any remain — pushing past it is a workflow violation.

## Layer 3: PR create (steps 3.L3.1 through 3.L3.3)

Push the branch and open the PR. Three things to get right:

1. **Branch name** matches the project's `branch_format` (firefly-pro: `gary/{prefix}-{number}-{description}` from project config).
2. **PR body contains `Ref FPCRM-XXX`** (literal string), NOT `Fixes` / `Closes` / `Resolves`. Linear's native integration auto-transitions on the latter three, bypassing the QA-Testing handoff this skill needs.
3. **PR is linked to Linear via `mcp__linear__link_attachment_github_pr`** so the issue's Attachments panel shows the PR.

### Draft PRs

For work that's not ready for human review (still iterating on Layer 4 fixes), open as `--draft`. Convert to ready (`gh pr ready`) before merge. The CI gate at L4 doesn't care about draft state; the merge gate at L5 does.

## Layer 4: CI + CodeRabbit (steps 3.L4.1 through 3.L4.3)

Wait for the project's CI to finish, then read and triage CodeRabbit's review comments.

### Diagnosing CI failures

Don't just retry CI hoping it goes green. Read `gh run view --log-failed` for the failing job and the Blacksmith analytics report (per project config). Fix the root cause, push the fix, re-check. Max 3 iterations before the **Circuit Breaker** trips (M4.4 / commit 880eb27) — if CI is failing 3+ times in a row, something deeper is wrong than a flaky test. Stop, escalate.

### CodeRabbit triage

CodeRabbit comments fall into three buckets:

| Bucket | Action |
|--------|--------|
| **Actionable** | Real bug or material improvement. Fix in this PR. |
| **Nitpick** | Style preference, micro-optimization, taste issue. Acknowledge with one-line response, don't fix. |
| **False positive** | CodeRabbit misread the code. Reply explaining why it's wrong. |

Never *ignore* a CodeRabbit comment — every one gets a response. Dismissing without reasoning is what review-fatigue looks like; future you (or a teammate) reading the PR history needs to know whether each comment was considered.

## Layer 5: Final gate + merge (steps 3.L5.1 through 3.L5.2)

Verify all earlier layers' gates passed (recorded as booleans in the chain's artifact stream — `layer_0_gate`, `layer_1_gate`, ... `layer_4_gate`). If any is false, refuse to merge.

### Merge strategy

Per the firefly-pro project config: squash for feature work, merge commits for releases. Use `mcp__github__pr_merge`.

### What happens at merge

The `linear-on-merge.yml` workflow fires (per CLAUDE.md). It parses every `Ref FPCRM-XXX` reference in the PR title and body, looks up each referenced issue, and if the issue is in `Code Review`, transitions it to `QA Testing` and reassigns it to the QA tester. The phase 3.5 (qa-handoff) docs cover the downstream side.

## What the AI judge looks for

The judge for this phase has more nuance than the claim-phase judge:

- **Test correctness.** No regressions vs baseline. New tests exercise the new behavior (heuristic: at least one test file modified per non-test file modified, with exceptions for pure refactors).
- **Review attention.** Every Codex `critical`/`high` finding has either a fix in the diff or an explicit dismissal with reasoning.
- **PR hygiene.** Body contains `Ref FPCRM-XXX`, not auto-closing keywords. Linked to the Linear issue. Branch name matches project format.
- **CI cleanliness.** All required checks green at merge time. Max 3 CI fix iterations (Circuit Breaker).

Judge does NOT evaluate code quality directly — it evaluates whether the *review process* was followed. Code quality is the lens agent's and Codex's job; the judge just checks that the process produced a defensible verdict.

## Edge cases

### Codex disagrees with the lens agent

This is *meaningful*. M3.3's multi-lens verdict structure plus M5.0's pluggable judge backends (#69 Stage A+B) explicitly track cross-vendor disagreement. Flag for human review; don't suppress the disagreement. A `DisagreementMarker` lands in the proof chain (commit 162c7d3).

### CI green but CodeRabbit found an actionable issue

CI doesn't know about CodeRabbit. They're orthogonal. The L4 gate requires BOTH: all CI checks green AND all CodeRabbit actionable comments addressed (fixed or dismissed with reasoning).

### A new test added at L0.4 fails

Self-inflicted: the test exercises behavior the implementation doesn't yet support. Either fix the implementation (back to Phase 2.7) or mark the test `#[ignore]` with a TODO and a follow-up issue (don't ship `#[ignore]` without a follow-up — that's how tests rot).

### Layer 5 finds a defect

This means earlier layers missed something. Don't just fix it and merge — file a meta-issue for *why the earlier layers missed it* so the rubric gets tightened. Each defect found at L5 is feedback that L0-L4 needs.

## Related references

- `claim-phase.md` — Phase 0, what came before this.
- `linear.toml` — the canonical step list.
- The `CONTRIBUTING.md` Apollo design principles section explains the underlying philosophy (commit cbcb735).
