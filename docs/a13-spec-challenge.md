# A13 — Spec-Challenge Before Execute

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **A13** (A-tier; addresses Cemri's largest failure cluster)
**Related:**
- `docs/a3-dry-run-then-commit.md` (A3) — A13 fires *before* A3 in the gate pipeline; the challenge produces a clarified spec that A3's dry-run then operates on
- `docs/a6-reversibility-graded-tripwires.md` (A6) — A13's trigger is reversibility-class-graded (Irreversible+ requires challenge; reversible may opt in)
- `docs/policy-no-auto-summary-without-critique.md` (R8) — A13 is the upstream check that prevents the "we executed the wrong thing correctly" failure mode R8's BA-vertical specialization (BA5) targets at the output stage
- `docs/ba1-ba3-sentinel-enforcement.md` — BA3's requirements traceability gate works with A13: requirements emerge from challenge, A13 produces refs, BA3 enforces traceability
- Future: BA2 (two-mode discovery) + BA4 (stakeholder interrogation protocol) — A13's challenge is the in-session, small-scope version of BA2/BA4's larger interrogation protocol
- Memory: `architecture-hexagonal-ddd`

---

## TL;DR

A13 adds a **mandatory `claim → challenge → verify → execute`** workflow step where the acting agent must articulate, before any irreversible action, what the spec is *unclear about*, what assumptions it is *making*, and what alternatives it considered. A *separate-model-family challenge agent* scores the challenge for completeness; missing challenge categories (no alternatives considered, no assumptions surfaced, no acknowledgment of constraints) block.

This addresses **Cemri et al. (2025) "Why Do Multi-Agent LLM Systems Fail?" cluster 1 — specification ambiguity**, which the authors identify as the single largest source of multi-agent failure. The brief calls it out as a structural-impossibility primitive: with A13, "I built the wrong thing correctly" becomes structurally impossible because the agent had to articulate what the right thing was before acting.

A13 is *smaller in scope* than A3 — it doesn't require expensive auditor calls or human sampling. It's a deterministic check on a structured artifact the agent itself produces. Cost is roughly one extra agent turn before irreversible actions; benefit is closing the most-frequent multi-agent failure cluster.

---

## 1. The architectural problem

Sentinel's existing skill workflow is `claim → verify → execute`. The phases produce proofs but make no demand that the agent *examine its own spec interpretation*. The agent reads the user's prompt, internalizes some interpretation, and proceeds. If the interpretation is wrong, the failure is detected (if at all) by:

- **R8 / BA5** — at the output stage, by adversarial critique reading the artifact. By then the work has been done; the failure is recoverable only in proportion to the action's reversibility.
- **A3** — at the dry-run stage, by an auditor scoring the proposed mutation. The auditor sees the *action*, not the agent's interpretation of *why*. Misinterpretations that produce plausible-looking actions slip through.
- **Operator review** — eventually. Sometimes after a deliverable has shipped.

None of these catch "I built the wrong thing correctly" at the *moment of interpretation*, which is the cheapest place to catch it.

A13 inserts a phase before execution where the agent must:
- Restate the spec in its own words.
- Enumerate the assumptions it's making.
- Identify what the spec doesn't say (gaps).
- Identify what the spec could mean (ambiguities).
- Name alternatives it considered (and dismissed — with reasoning).
- Flag constraints it's *not* satisfying (and why).

The challenge artifact is structured. A small deterministic check validates completeness. A challenge agent scores semantic quality. If the challenge is empty ("I have no questions; I'll proceed") for an Irreversible+ action, that itself is a failure signal — real work this complex without any open questions is the diagnostic shape of an agent operating on confident misreading.

---

## 2. The challenge artifact

```rust
// In sentinel-domain/src/spec_challenge.rs (new module)
pub struct SpecChallenge {
    pub work_id: WorkId,
    pub agent_id: AgentId,
    pub challenged_spec: SpecReference,     // hash + source of the spec being challenged
    pub reversibility_class: ReversibilityClass,

    // The five required categories — each may be empty *only* if the agent
    // explicitly asserts "none" with reasoning. Silent empties block.
    pub assumptions: ChallengeCategory<Assumption>,
    pub gaps: ChallengeCategory<SpecGap>,
    pub ambiguities: ChallengeCategory<Ambiguity>,
    pub alternatives_considered: ChallengeCategory<Alternative>,
    pub constraints_not_satisfied: ChallengeCategory<UnsatisfiedConstraint>,

    pub created_at: DateTime<Utc>,
}

pub struct ChallengeCategory<T> {
    pub items: Vec<T>,
    pub explicit_assertion_of_none: Option<String>,  // required if items is empty
}

pub struct Assumption {
    pub statement: String,
    pub confidence: AssumptionConfidence,  // Low | Medium | High
    pub blast_if_wrong: ReversibilityClass,
}

pub struct SpecGap {
    pub topic: String,
    pub how_resolved: GapResolution,  // OperatorClarified | InferredFromContext | DefaultApplied
    pub inference_source: Option<String>,  // required if InferredFromContext
}

pub struct Ambiguity {
    pub spec_excerpt: String,
    pub interpretations: Vec<String>,
    pub chosen: String,
    pub rationale: String,
}

pub struct Alternative {
    pub description: String,
    pub why_rejected: String,
}

pub struct UnsatisfiedConstraint {
    pub constraint: String,
    pub why_not_satisfiable: String,
    pub workaround: Option<String>,
}
```

The artifact is *structured*. It is not free-text reflection. The structure forces the agent to *categorize* its uncertainty rather than handwave it.

---

## 3. The trigger — reversibility-graded

A13 fires based on the reversibility class of the upcoming work (per A6):

| Reversibility class | A13 behavior |
|---|---|
| TriviallyReversible | Skipped — challenge cost not justified for trivial actions |
| ReversibleWithEffort | Optional — agent may emit a challenge artifact; not required; no block |
| Irreversible | **Required** — challenge artifact must be produced and pass completeness check |
| Catastrophic | **Required + multi-axis scoring** — challenge produced, scored by a separate-model-family challenge agent, *all five categories* must score above threshold |

For BA-vertical work specifically (per the planned BA2 + BA4), A13 also fires at the *start of an orchestration*, not just before each mutation. The orchestration-level challenge is the structural form of "interrogated discovery" — what's unclear about the stakeholder's need *before* we go gather data.

---

## 4. The completeness check

A deterministic validator runs against every challenge artifact:

```rust
fn validate_challenge_completeness(challenge: &SpecChallenge, class: ReversibilityClass) -> ValidationResult {
    let mut issues = Vec::new();

    for category in &[
        ("assumptions", &challenge.assumptions),
        ("gaps", &challenge.gaps),
        ("ambiguities", &challenge.ambiguities),
        ("alternatives_considered", &challenge.alternatives_considered),
        ("constraints_not_satisfied", &challenge.constraints_not_satisfied),
    ] {
        let (name, cat) = category;
        if cat.items.is_empty() && cat.explicit_assertion_of_none.is_none() {
            issues.push(format!(
                "Category '{}' is empty without explicit assertion of none. \
                 Silent empties are blocked; if there really are no items, \
                 say so explicitly with reasoning.",
                name
            ));
        }
        // For Catastrophic class: requiring at least one item per category is too rigid;
        // explicit assertion of none must include detailed reasoning (length > 50 chars)
        if class == ReversibilityClass::Catastrophic {
            if let Some(assertion) = &cat.explicit_assertion_of_none {
                if assertion.len() < 50 {
                    issues.push(format!(
                        "Catastrophic-class challenge requires detailed reasoning \
                         for empty category '{}' (got {} chars; need >50).",
                        name, assertion.len()
                    ));
                }
            }
        }
    }

    // Each Assumption must have a blast_if_wrong; if any assumption has
    // blast_if_wrong == Catastrophic, the work itself is catastrophic-class
    // regardless of what A6 originally classified.
    let catastrophic_assumptions = challenge.assumptions.items.iter()
        .filter(|a| a.blast_if_wrong == ReversibilityClass::Catastrophic)
        .collect::<Vec<_>>();
    if !catastrophic_assumptions.is_empty() && class < ReversibilityClass::Catastrophic {
        issues.push(format!(
            "{} assumption(s) have blast_if_wrong=Catastrophic but the work is \
             only classified as {:?}. This is a class promotion signal — the \
             work should be re-classified up.",
            catastrophic_assumptions.len(), class
        ));
    }

    if issues.is_empty() {
        ValidationResult::Pass
    } else {
        ValidationResult::FailWithIssues(issues)
    }
}
```

The completeness check is **deterministic** — same input, same output. No model call. Cheap. Fires before the more expensive semantic scoring (next section).

---

## 5. Semantic scoring (Catastrophic class only)

For Catastrophic-class work, the completeness-check pass triggers a semantic scoring pass by a **challenge agent** (a separate model-family agent picked via A2's `CapabilityRouterPort`).

The challenge agent scores each category on a small set of axes:

```rust
pub struct ChallengeScore {
    pub assumption_quality: f32,         // are the assumptions specific and falsifiable?
    pub gap_identification: f32,         // are the gaps real spec gaps vs trivia?
    pub ambiguity_resolution: f32,       // are interpretations enumerated, not collapsed?
    pub alternative_seriousness: f32,    // are rejected alternatives steelman'd?
    pub constraint_honesty: f32,         // are unsatisfied constraints surfaced rather than hidden?
}
```

Each axis 0.0-1.0. Threshold for catastrophic-class is 0.7 average across axes; sub-threshold blocks; threshold-pass proceeds.

The challenge agent reads the spec, the challenge artifact, and the recent transcript context. It does *not* read the proposed action — that's A3's auditor's job. The two checks are orthogonal: A13 validates the *interpretation*; A3 validates the *action chosen for the interpretation*.

---

## 6. Phase integration — `claim → challenge → verify → execute`

Sentinel's existing skill workflow becomes:

```
claim    → agent declares "I'm going to do X"
challenge → A13 fires: agent emits SpecChallenge artifact; validator + (for Catastrophic) scorer pass
verify   → existing verify phase (proof of preconditions met)
execute  → agent acts; A3 dry-run-then-commit fires inside execute for Irreversible+
```

The `challenge` phase produces a `PhaseProof` of type `SpecChallengeProof` carrying the artifact and the validation/scoring results. The proof chain links it to the surrounding claim and verify proofs.

For non-skill mutations (PreToolUse hook firing on a raw Edit/Write/Bash/MCP outside a skill), A13 wraps the tool call directly:

1. PreToolUse fires.
2. `tool_usage_gate` passes (per A6 classification).
3. A13 fires: agent is asked to emit `SpecChallenge`. If the agent hasn't emitted one in the recent transcript (lookback window), the tool call blocks with a clear message: "Emit a SpecChallenge before this action; the action is class X."
4. Once challenge is emitted and validated, A13 records the approval marker and the tool call proceeds to A3.

The lookback window means one challenge can cover multiple related mutations in the same context — a Catastrophic challenge approves a sequence of related actions within the same hash of (work_id, agent_id, spec_hash), not just one mutation.

---

## 7. Interaction with the BA-vertical

### 7.1 Orchestration-start challenges

When a BA-orchestration begins, A13 fires at the orchestration level, not just before mutations. The challenge categories are interpreted with BA-specific semantics:

- **Assumptions** — what is the BA agent assuming about the stakeholder's intent?
- **Gaps** — what's missing from the stakeholder's request that data collection will need to fill?
- **Ambiguities** — what could the request mean? Which interpretation drives the rest of the work?
- **Alternatives** — what alternative deliverables would also satisfy the request? Why pick the one chosen?
- **Constraints not satisfied** — what about the request is *not going to be fully addressed* in this orchestration (and why)?

This is the **structural primitive that BA2 (two-mode discovery) and BA4 (stakeholder interrogation protocol) build on**. A13's challenge artifact at orchestration start *is* the initial interrogation plan; BA4's protocol expands the gaps into structured stakeholder questions; BA2 routes them to automated discovery (BA6 connectors) or interrogated discovery (BA4 batched questions).

### 7.2 Per-recommendation challenges

Before each recommendation in a BA output, the orchestrator emits a small per-recommendation challenge. Catches "this recommendation is one of several reasonable interpretations of the stakeholder's need; I should surface the others." Becomes input to BA5's critique axis 3.2 (missing alternatives).

---

## 8. Hex / DDD layering

- **`sentinel-domain/src/spec_challenge.rs`** (new): all `SpecChallenge`, `ChallengeCategory<T>`, `Assumption`, `SpecGap`, `Ambiguity`, `Alternative`, `UnsatisfiedConstraint`, `ChallengeScore`, `ValidationResult` value objects. Pure data.
- **`sentinel-domain/src/ports/challenge_scorer.rs`** (new): `ChallengeScorerPort` trait — `score(challenge: &SpecChallenge) -> Result<ChallengeScore, _>`. Used only for Catastrophic class.
- **`sentinel-application/src/hooks/spec_challenge_gate.rs`** (new): the hook. Runs in the `challenge` phase of the skill workflow, OR wraps non-skill PreToolUse for Irreversible+ actions.
- **`sentinel-application/src/workflow/`**: existing workflow module gains the new phase between `claim` and `verify`.
- **`sentinel-infrastructure/src/challenge_scorer/`**: adapter implementations. Reuses A3's `AuditorPort` adapters with a challenge-scoring prompt template — *via* the A2 router, which picks an agent matching the `ChallengeScorer` capability requirement.

All hex/DDD-respecting per `[[architecture-hexagonal-ddd]]`. The `ChallengeScorerPort` is a pure trait. The scorer adapters are in infrastructure. The deterministic completeness check is pure logic in `sentinel-domain`.

---

## 9. Failure modes

### 9.1 Agent emits a perfunctory challenge

Empty categories with one-line explicit-assertion-of-none: completeness check warns but doesn't block (the agent has the option to insist there are no items). For Catastrophic, the >50-char reasoning requirement raises the floor. For sustained low-quality challenges, sentinel emits a health warning to the operator.

### 9.2 Agent learns to produce challenges that pass the validator

This is the R5-quarantine boundary. The completeness check is *deterministic* (no learned signal). The semantic scorer for Catastrophic is a learned signal, but per R5 the score must not become a training input for the acting agent. If the scorer's output becomes a selection pressure on the actor's behavior, the system is back to deceptive-alignment territory.

Mitigation: the scoring agent and the acting agent share no training data; the operator monitors agreement between challenge quality and downstream outcome quality; honeypot canaries (A7) plant challenges with deliberate gaps to verify the scorer catches them.

### 9.3 Challenge categories interpreted as legalistic compliance

If agents start writing challenges in a stilted bureaucratic way to satisfy the format, the artifact becomes noise. Mitigation: format examples shipped with prompt templates; operator-facing reports surface challenge quality trends; sustained quality decay triggers operator review.

### 9.4 The challenge process slows everything

A13 fires for Irreversible+ work, not for routine mutations. The cost is bounded by the rate of irreversible actions (which is low for typical workflows). For BA-vertical work, the orchestration-start challenge is one extra agent turn at the beginning; perceived as discovery work, not gate friction.

Override available per the standard `hygiene_override` mechanism for legitimate edge cases.

### 9.5 Stakeholder doesn't actually want to answer the gaps

A13 produces gaps; BA4 (when ratified) turns them into stakeholder questions. If stakeholders don't engage, BA4's protocol times out and the agent proceeds with explicit assertions of "inferred without confirmation." Audit trail records the inference; downstream BA5 critique flags inference-without-confirmation as a warning.

---

## 10. Test strategy

- **Unit tests in `sentinel-domain/src/spec_challenge.rs`**: validation logic for each category; Catastrophic-class stricter rules; assumption blast-promotion (Catastrophic blast → class promotion).
- **Hook tests**: agent emits valid challenge → proceed; agent emits empty challenge → block with clear message; agent emits Catastrophic challenge with short reasoning → block; challenge in transcript covers subsequent related action within window.
- **ChallengeScorerPort mock**: in-memory scorer returning canned scores; threshold logic validated.
- **Workflow integration test**: skill execution with `claim → challenge → verify → execute`; each phase produces correct proof; chain re-verifies cleanly.
- **BA-orchestration test**: orchestration-start challenge produced; gaps enumerated; downstream discovery work references the gap IDs from the challenge.
- **Override test**: `hygiene_override` for an Irreversible action bypasses A13; audit records the bypass with reason.

---

## 11. Open questions

1. **Where in the agent's response does the challenge go?** Recommend a structured tool call: agent calls `mcp__sentinel__emit_challenge` (new — adds to sentinel-mcp's tool surface) with the structured artifact. Distinct from free-text in the response so the validator can parse reliably.

2. **Per-category importance weighting.** Should some categories carry more weight than others? (E.g., "alternatives_considered" matters more than "constraints_not_satisfied" for greenfield BA work.) Recommend uniform weights v1; operator-tunable per workflow as a v2 enhancement.

3. **Lookback window for tool-call coverage.** How long does a challenge approve subsequent actions? Recommend: 10 minutes or until the work_id / spec_hash changes, whichever first.

4. **Interaction with Legatus AI peer registration (ADR-016).** When a Legatus AI peer issues a directive, does the peer's identity influence A13's strictness? (E.g., human-operator peers might be trusted to operate without challenge for some Irreversible actions.) Recommend no — challenge is about the agent's *interpretation*, not the directive's *source*. Human operators can still override.

5. **Challenge artifact size limits.** A pathologically verbose challenge could exceed reasonable size. Recommend: per-category item count caps (default 20 per category); reasoning text length caps (1000 chars per item); operator-tunable.

---

## 12. Decision and ownership

- **Decision class:** sentinel architectural change. Adds a value-object module, a port, a hook, a new skill-workflow phase, and one new sentinel-mcp tool.
- **Owner:** Gary Somerhalder ratifies. Compatible with A3 (A13 fires before A3; they're orthogonal — A13 validates interpretation, A3 validates action). Substrate for BA2 + BA4 (the BA-vertical's larger interrogation protocol builds on A13's per-action challenges).
- **Re-evaluation cadence:** revisit after 1000 challenges accumulated — calibrate category thresholds, weight tuning, BA-specific specialization.
- **Related items in the brief:** A13 (this), A3 (parallel pre-execute check; orthogonal), A6 (reversibility classification drives the trigger), R8 (the retirement A13 structurally upstream-prevents), BA2 + BA4 (substrate), BA5 (consumer — alternative quality feeds critique axis 3.2).

---

## 13. Methodology caveat

The primary evidence is **Cemri et al. (2025) "Why Do Multi-Agent LLM Systems Fail?"** (verify ID; commonly cited in the 2025 multi-agent literature) — clusters multi-agent failures into specification ambiguity, inter-agent misalignment, and verification gaps. Specification ambiguity is the largest cluster; A13 addresses it structurally. ArXiv ID from training-data recall (cutoff January 2026); verification required before external publication.

## 14. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building the `spec_challenge_gate` hook + `SpecChallenge` value objects + `ChallengeScorerPort`.
- Adding the `challenge` phase to the skill workflow.
- Adding `mcp__sentinel__emit_challenge` to sentinel-mcp's tool surface.
- Treating R5 boundary as load-bearing: challenge scoring is dispatch input + audit signal, never training input for the actor.
