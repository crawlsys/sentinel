# Linear skill — claim phase (steps -0.1 through 0.4)

Reference for the 8 steps in `config/steps/linear.toml` under `[[phases]] id = "claim"`. The phase moves an issue from `Backlog` to `In Progress` with the agent as assignee, after first verifying the issue is actionable, unassigned, not a duplicate, and owned by the correct team.

## Why this phase exists

The very first action when picking up a Linear issue must be to claim it — set state `In Progress` and assignee = viewer. Anything else (reading description, planning, opening a worktree) before claiming creates a race: two developers can start work on the same ticket. **Claim first, plan second.** The linear skill enshrines this as Phase 0; the `-0` (pre-claim) sub-phase exists so we don't claim an issue we shouldn't have touched (already owned, duplicate, wrong team).

## Sub-phase guard rails (steps -0.1 through -0.4)

These four steps are blockers (failure stops the phase). They are **read-only**: no `mcp__linear__update_issue` calls. They establish that proceeding is safe.

| Step | Question it answers |
|------|--------------------|
| `-0.1` | Does the issue exist, in a state we can work on (not archived/canceled/done)? |
| `-0.2` | Is someone else already assigned and actively working it? |
| `-0.3` | Is this a duplicate of an existing issue, or obvious spam? |
| `-0.4` | Is the issue's team key (e.g. `FPCRM`) one this project actually owns? |

A failure in any of these is **not** an error — it's evidence the agent should pick a different issue. Phase aborts cleanly.

### -0.3 duplicate detection

The naïve heuristic: search Linear for the first 6-10 words of the title. If the top 1-2 results have ≥70% title overlap AND are in a "live" state (anything except canceled/duplicate), flag as duplicate. The agent should NOT auto-mark either as duplicate of the other — that's a human decision. It just refuses to claim and surfaces the candidates.

### -0.4 team ownership

The project config (`~/.claude/sentinel/projects/firefly-pro.md`) declares `linear_team_key: FPCRM`. If the issue's `team.key` doesn't match, this isn't our work. Common false-positives: a developer cross-posting a `FPFIELD` issue to `FPCRM`. Refuse and ask the developer which project the work actually belongs to.

## Claim execution (steps 0.1 through 0.4)

Once guards pass, the actual transition is 4 steps:

1. **0.1 Look up "started" state ID by type.** `mcp__linear__get_workflow_states(team_id)` returns all states; find the one with `type == "started"` (usually named "In Progress" but state names are project-customizable). **Never hardcode "In Progress" as a string** — projects rename states; the `type` field is stable.
2. **0.2 Get viewer.** `mcp__linear__viewer` returns the authenticated user's record. With per-developer Doppler personal-branch identity (commits 90/94 + post-#100), this resolves to the developer running the session, not a shared bot.
3. **0.3 Set In Progress + assign self.** Single `update_issue` call carrying both `state_id` and `assignee_id`. Atomic in Linear's API — either both land or neither does, no partial state.
4. **0.4 Confirm.** `get_issue` again, assert state name and assignee match what 0.3 set. Catches the case where Linear's webhook latency makes the update visible-but-not-yet-readable; if this fails, retry 0.3 once with a 2s delay before bailing.

## What the AI judge looks for

The judge for this phase evaluates two things:

1. **Identity correctness.** The assignee on the final issue must be the developer running this session, not a service account. With Doppler personal-branch identity, this is a check that `viewer.email` matches the developer's expected email.
2. **State semantics.** Issue state must be a `type == "started"` state, not just *named* "In Progress." (Phase 3.5 transitions to QA Testing via `type == "review"` — same pattern.)

Judge does **not** evaluate whether the issue *should* have been claimed — that's the -0 sub-phase's job. The judge takes the guards' verdict as given.

## Edge cases

### The agent is already assigned

If `-0.2` finds the agent is the existing assignee AND state is already `In Progress`, the phase is a no-op. Skip to phase 1 (Fetch). Don't fail — this is the resume-after-restart case.

### The issue is "In Progress" but assigned to someone else

That's a real conflict. -0.2 refuses. Surface clearly: "FPCRM-1234 is assigned to <other dev> and in progress. Coordinate with them or pick a different issue."

### Linear API rate limit during 0.3

Linear's rate limit is generous but bursty agents hitting it during batch operations is real. Retry once after 30s. If the second attempt also rate-limits, surface the error and bail — don't loop indefinitely.

### State name was renamed to something non-obvious

Linear lets workspaces rename `In Progress` to whatever they want. The lookup-by-type at 0.1 handles this — but if the agent or a script hardcodes the string anywhere, it breaks silently. The schema validator (#103) doesn't catch this; runtime does.

## Related references

- `review-phase.md` — Phase 3, the post-implementation pipeline.
- `linear.toml` — the canonical step list with `artifact_schema` and `suggested_tools`.
