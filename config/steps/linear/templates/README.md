# `config/steps/linear/templates/`

**Pattern:** Pre-baked virtual skill packs (M2.14, task #27). Each TOML in this directory is a **template** — an ordered list of step references that compose a multi-phase recipe the router can offer as a quick-start instead of planning from scratch.

Templates are NOT executable directly. They're suggestions: when the router (M7.3 / #52) sees a prompt that matches a template's `triggers`, it can propose the template as the plan rather than reasoning over the full 91-step linear skill from scratch. The user approves or edits, the resulting `VirtualSkillPack` (M7.2 / #51) drives execution.

## Schema

```toml
[template]
id = "claim-and-pr"                   # filename stem matches
name = "Claim and open PR"
description = "Pick up a Linear issue, claim it, implement minimal change, push as draft PR."
triggers = [                          # phrases the router matches against
    "claim and pr",
    "pick up issue and open pr",
    "draft pr",
]
estimate_minutes = 30                 # rough wall-clock for the user
catalog = "default"                   # which router catalog this lives in (M7.8/#57)

[[template.steps]]
ref = "linear.claim.-0.1"             # skill.phase.step_id
[[template.steps]]
ref = "linear.claim.-0.2"
# ... etc
```

The `ref` strings name `(skill, phase_id, step_id)` triples. Validator (#103's natural follow-up) checks each `ref` resolves to a real step in the named skill's TOML.

## When to add a template

A template earns its place when:

1. **A multi-step recipe gets used often enough that re-planning is wasted work.** "Claim an issue and open a draft PR" is the canonical example — most issue-pickup sessions do exactly that and not the full 91-step pipeline.
2. **The recipe has a clean stop point.** "Claim and PR" stops at PR open (no merge, no QA). "Full ship" stops at Completed. Half-recipes that bleed into ad-hoc work later don't make good templates.
3. **The set of steps is stable.** If you find yourself editing the template every other use, it isn't really a template — it's a starting point you should plan from. Move it to a doc, not here.

## When NOT to add a template

- **Cross-skill recipes belong in a different layer.** Template files here are scoped to one skill (linear). Cross-skill packs (e.g. "claim a linear issue AND post a slack notification") live in the router's catalog directly (M7.8 pack contracts) once that lands.
- **Steps with conditional branches.** The template format is linear (ordered list). Branching plans are a M7.4 LangGraph concern. If a "template" needs an `if`, it isn't a template — it's a plan.

## What lives here today

| Template | Description | Steps |
|----------|-------------|-------|
| `claim-and-pr.toml` | Claim → fetch → intelligence → worktree → review L0-L3 (stops at PR open, draft). | 44 |
| `quick-comment.toml` | Read issue + acknowledge prior comments + post status comment. No state change. | 4 |

A `review-merge-deploy.toml` (full 91-step pipeline) was considered and skipped — it duplicates `linear.toml`'s step list verbatim, adds no information value. The router can compose the full pipeline by referencing every step in `linear.toml` directly when needed.

## How the router consumes templates (preview — M7.3 not landed yet)

When M7.3 (router-as-planner, #52) ships, the planner reads templates at startup and matches user prompts against `triggers`. A match emits a `VirtualSkillPack` populated from `template.steps` rather than reasoning over the full step list. The pack is shown to the user for approval (M7.7 frozen-packs gate / #56 covers production-critical paths where this is mandatory).

Until M7.3 lands, templates are documentation-only — useful for humans who want to know "what's the canonical recipe for X."

## Related references

- `../references/` — per-phase long-form docs (M2.10, #23 closed).
- `linear.toml` — the canonical step list templates `ref` into.
