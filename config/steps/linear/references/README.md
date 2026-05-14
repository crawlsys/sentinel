# `config/steps/linear/references/`

**Pattern:** Apollo Skills' progressive-disclosure split (M2.10, task #23). Short, schema-bearing data lives in `config/steps/linear.toml`. Long-form prose — examples, edge cases, rubric for the AI judge, agent guidance that would bloat tool schemas — lives here, one file per phase.

The TOML is what skills-mcp's build.rs codegen reads to emit `#[tool]` handlers. Anything in this directory is auxiliary: read by humans, optionally by AI judges that need rubric context, never by codegen.

## Layout

```
config/steps/
├── linear.toml                       # Schema (always loaded)
└── linear/
    └── references/                   # Detail (loaded on demand)
        ├── README.md                 # This file
        ├── claim-phase.md            # Phases -0 + 0
        ├── review-phase.md           # Phase 3, the 6-layer pipeline
        └── (more as needed)
```

## When to put something here vs in the TOML

| Goes in `linear.toml` | Goes in a reference file |
|----------------------|--------------------------|
| Step ID, blocker flag | Rationale for *why* a step is a blocker |
| `description` (one line) | Multi-paragraph explanation, examples |
| `suggested_tools` array | Walkthroughs of each tool's usage in context |
| `artifact_schema` (one-line typed field shape) | What the artifact *means*, edge cases |
| Federation directives (`provides`, `requires`, `@deprecated`) | Migration history, deprecation rationale |
| `judge_model`, baseline thresholds | Rubric the judge applies, score interpretation |

If you're tempted to write a multi-line description in TOML, that's the signal to move it here and link from the TOML's one-line description.

## Linking from TOML (future schema integration)

Today, references are discoverable by path convention only — the TOML doesn't declare them. The natural follow-up adds an optional `references = ["filename.md"]` field to `WorkflowStep`. Until that schema field lands:

- Reference filenames mirror phase IDs: `claim-phase.md`, `review-phase.md`, etc.
- Skill authors update both the TOML description AND the matching reference in the same change. The validator in #103's path could grow a "every reference filename matches a real phase ID" check.

## Why not just one big README?

Phase files are bounded — claim is ~8 steps, review is ~22 steps. Each fits comfortably in one file. The boundary is the phase, not the skill, because:

- The AI judge for a *single phase* is the natural consumer of *that phase's* rubric.
- Editing one phase doesn't churn the others' files in PR diffs.
- A new contributor reading "how does the review phase work" finds it in one place, not in skim-mode across a 2000-line monolith.

A single huge `linear-reference.md` would have the same content but worse signal-to-noise for the typical reader.
