# BA5 — Presentation Generation with Adversarial Critique

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **BA5** (S-tier; product-critical for the BA vertical)
**Related:**
- `docs/policy-no-auto-summary-without-critique.md` (R8) — BA5 is the constructive complement; R8 retires the unchecked pattern, BA5 specifies the replacement
- `docs/a3-dry-run-then-commit.md` (A3) — BA5 specializes A3's auditor pattern for BA-vertical outputs; A3 supplies the substrate, BA5 supplies the BA-domain critique axes
- `docs/ba6-connector-layer-scoping.md` (BA6) — BA5 depends on BA6 for citation provenance; BA1's `ArtifactReference` shape is what critique can verify against
- `docs/policy-replay-mining-quarantine.md` (R5) — BA5 must hold under the R5 quarantine; critique pass/fail traces must not become an auto-training signal
- Legatus AI ADR-017 (Artifact + Requirement Metadata Extensions) — defines `ArtifactReference` + `RequirementRef`; BA5 reads both during critique
- Memory: `architecture-hexagonal-ddd`

---

## TL;DR

Sentinel does not ship any BA-vertical output (deck, brief, memo, exec summary) to a human reader without an **adversarial critique** attached. Critique and artifact travel together; the human always sees both side-by-side. This is the **constructive complement to R8** (which retires unchecked auto-summary) and the **BA-vertical specialization of A3** (which supplies the separate-model-family auditor pattern).

BA5 is the highest-trust gate in the BA-vertical product. Execs read decks. A confident-sounding deck without critique becomes a business decision. Getting this wrong is paid in unrecoverable trust loss the first time a hallucinated finding causes a real-world commitment. The cost of compliance — one extra model invocation per output, plus the UX commitment to render critique alongside artifact — is a fraction of the cost of any single trust-loss event.

The architecture is small. The discipline is the load-bearing part.

---

## 1. What BA5 is

From the source brief (recommendation BA5):

> Refines R8 specifically for BA outputs. Every deck, brief, or memo passes through an adversarial reviewer agent whose job is to find unsupported claims, missing alternatives, citation gaps, and tonal spin. The critique attaches to the artifact and the human reviewer sees both.

Operationally, the rule is:

1. **Every BA-vertical output destined for a human reader** triggers BA5 — decks, briefs, memos, exec summaries, written recommendations, report rollups generated for human consumption.
2. **The artifact passes through a separate-model-family critique agent** (A3's auditor seat, with BA-vertical critique axes) before it is rendered or delivered.
3. **The critique becomes a structured artifact** attached to the output. It is not optional metadata; it is a sibling document.
4. **The human reader is shown both** side-by-side. The presentation layer must not surface the artifact without the critique.
5. **For catastrophic-class outputs** (exec decks, customer-facing recommendations, board materials), the human reviewer is *also* asked to acknowledge the critique before the artifact is considered delivered.

The pattern is the same shape as A3's dry-run-then-commit. The differences are: the trigger is BA output (not just any mutating action), the critique axes are BA-vertical (not just intent alignment), and the "commit" is "render/deliver to human" (not just file mutation).

---

## 2. Trigger — what counts as a BA output for BA5

BA5 fires on three classes of artifact, listed by escalating consequence:

### 2.1 Internal artifacts (BA-routine class)

- Working notes the operator uses for their own reasoning.
- Intermediate analyses surfaced inside a single workflow.
- Read-only summaries of source artifacts (no recommendation embedded).

Critique still attaches, but is lightweight — single-pass, no human acknowledgment required. Operator can review the critique async.

### 2.2 Decision-shaping artifacts (BA-substantial class)

- Recommendations, briefs, memos written for a stakeholder who will act on them.
- reports generated for a recurring review cycle.
- Written analyses cited in other artifacts.

Critique is full-pass. Human reviewer (the operator, before forwarding) sees both. Operator can override (with audit) if the critique is mistaken.

### 2.3 Catastrophic-class artifacts

- Exec-facing decks (CEO/CFO/COO/CRO audience).
- Board materials.
- Customer-facing recommendations or proposals.
- Anything that names dollar amounts, headcount changes, strategic pivots, or external commitments.

Critique is full-pass. **Two-eyes rule**: the operator AND the audited critique BOTH must clear the artifact before delivery. The human acknowledgment is logged in the proof chain; "I read the critique and accept the trade-offs" is the explicit step. No silent override.

The classification rides on the same reversibility-class machinery as A3 (separate ADR per the hook-quality bridge), with BA-specific extensions for output classification (audience, distribution, persistence).

---

## 3. The critique — what the auditor actually checks

Five axes. Each produces a score (0.0-1.0) and structured findings. The critique artifact carries all five.

### 3.1 Unsupported claims

Every assertion in the output is matched against the citation chain (per BA1 / `ArtifactReference`). Claims without a citation, or with citations whose `content_hash` no longer matches the source (stale or modified), are flagged.

- **Block-class finding**: claim with no citation at all.
- **Warn-class finding**: claim with stale citation (source has changed since retrieval).
- **Info-class finding**: claim cited but the citation paragraph doesn't textually support the claim (the auditor reads both and compares).

### 3.2 Missing alternatives

For any recommendation, the critique asks: "What alternatives were considered? Were they presented honestly?" The auditor compares the recommendation against the requirement (per BA3 / `RequirementRef`) and the source materials; the absence of alternatives is itself a finding.

- **Block-class**: recommendation made without any alternative considered.
- **Warn-class**: alternatives mentioned but dismissed in one sentence without analysis.
- **Info-class**: alternatives presented but evidently shaped to make the recommendation look stronger.

### 3.3 Citation gaps

Distinct from "unsupported claims" — this axis catches *whole sections* without any citations at all (purely generative content presented as analytical content). The critique scans citation density section-by-section.

- **Block-class**: a section longer than N words (configurable, default 150) with zero citations.
- **Warn-class**: section citation density falls below 1 citation per 200 words.

### 3.4 Tonal spin

The auditor reads the output for tonal patterns: hedging-when-claims-are-strong (false modesty), confidence-when-evidence-is-weak (the opposite — the dangerous one), corporate-euphemism for negative findings, persuasive language where neutral language would do.

- **Warn-class**: confident claims (e.g., "clearly the right choice", "the data is unambiguous") whose underlying citations are weak.
- **Info-class**: euphemism flagged ("opportunities for improvement" where "failures" is more accurate).

### 3.5 Requirements coverage

Every recommendation must trace to a stated business requirement (per BA3 / `RequirementRef`). The critique checks: are all stated requirements addressed? Are any requirements only superficially addressed? Are unstated requirements (inferred) flagged as such?

- **Block-class**: recommendation made without traceable requirement.
- **Warn-class**: requirement claimed addressed but the addressing paragraph doesn't actually engage with the requirement substance.
- **Info-class**: orchestrator inferred a requirement; inference should be made explicit to the reader.

---

## 4. The critique artifact

```rust
// In sentinel-domain/src/ba/critique.rs (new module)
pub struct BaCritique {
    pub artifact_id: ArtifactId,           // the output being critiqued
    pub artifact_class: BaArtifactClass,   // Routine | Substantial | Catastrophic
    pub auditor_model: String,             // e.g., "openai:gpt-5.5"
    pub axes: CritiqueAxes,
    pub findings: Vec<CritiqueFinding>,
    pub overall_recommendation: CritiqueDisposition,
    pub created_at: DateTime<Utc>,
}

pub struct CritiqueAxes {
    pub unsupported_claims: f32,
    pub missing_alternatives: f32,
    pub citation_gaps: f32,
    pub tonal_spin: f32,
    pub requirements_coverage: f32,
}

pub struct CritiqueFinding {
    pub axis: CritiqueAxis,
    pub severity: FindingSeverity,   // Block | Warn | Info
    pub location: ArtifactLocation,  // section / paragraph / slide / cell
    pub description: String,         // human-readable explanation
    pub suggested_fix: Option<String>,
}

pub enum CritiqueDisposition {
    Ship,              // no Block-class findings
    HumanReviewRequired,    // any Block-class finding OR catastrophic class
    BlockForRework,        // multiple Block-class findings; auditor recommends not shipping at all
}
```

The critique is its own artifact, hashed and joined to the source artifact via `artifact_id`. Both flow through the proof chain together.

---

## 5. Presentation — the human always sees both

The presentation layer (web UI, CLI, exported PDF, voice briefing) must render the critique alongside the artifact. Concrete contract for the BA-orchestrator's output renderers:

- **PDF / slide export**: each slide carries an inline marginal annotation linking to the critique finding(s) for that slide. The critique itself is appended as a final section.
- **Web UI**: split view with artifact on left and critique on right; collapsible but never default-hidden.
- **Voice briefing** (per BA5's intersection with Legatus AI's voice supervision): critique findings of Warn-class or higher are spoken; Info-class are summarized; the operator can ask "tell me the critique" at any point.
- **Email / chat delivery**: the critique appears in-line before the artifact, not as an attachment that can be ignored.

The rule is: **a critique that the human did not see is the same failure as a critique that did not happen.** Presentation discipline is part of the safety guarantee.

---

## 6. Two-eyes rule for catastrophic-class outputs

For exec-facing decks, board materials, customer-facing recommendations: the operator is the second pair of eyes. The flow:

1. BA-orchestrator generates artifact.
2. Sentinel triggers BA5; critique produced.
3. Artifact + critique routed to operator via Legatus AI.
4. Operator reads both. If critique surfaced Block-class findings, operator must either:
   - Accept the critique → BA-orchestrator reworks and returns to step 2.
   - Override the critique with explicit reason → audit-logged override, signed by operator's Legatus AI session.
5. Artifact delivered.

This is the same `hygiene_override` pattern A3 uses for catastrophic-class actions. The override is always available; the override is always audited; the audit makes "I knowingly shipped this with the critique flagging X" legible after the fact.

---

## 7. Hex / DDD layering

Mirrors A3's layering exactly — BA5 is A3 with BA-specific critique logic:

- **`sentinel-domain/src/ba/`**: new module. `BaCritique`, `CritiqueAxes`, `CritiqueFinding`, `CritiqueDisposition`, `BaArtifactClass`, `CritiqueAxis`, `FindingSeverity`, `ArtifactLocation` value objects. Pure data; no IO.
- **`sentinel-domain/src/ports/ba_critic.rs`**: new port `BaCriticPort` trait — `score(&self, artifact: &BaArtifact) -> Result<BaCritique, CritiqueError>`. Pure trait; sibling of `AuditorPort` from A3.
- **`sentinel-application/src/hooks/ba_critique.rs`**: new hook intercepting BA-artifact-publishing events (probably on PostToolUse for connector outputs flagged as BA-rendering, or via a new artifact-class registry). Routes to `BaCriticPort`, attaches critique to artifact, returns block decision if findings warrant.
- **`sentinel-infrastructure/src/ba_critic/`**: adapter implementations. The natural shape is a thin wrapper around A3's existing `AuditorPort` adapters with BA-critique prompt templates injected. Reuses A3's vendor-selection logic (auditor model differs from acting model).
- **`config/ba-classes.toml`** (new): per-artifact-type classification rules — what tool calls produce decks, briefs, memos, exec materials. Operator-extensible.

The critique-axis logic and the artifact-class taxonomy live in `sentinel-domain` — they are pure rules. The actual model call lives in `sentinel-infrastructure`. The hook orchestrates.

---

## 8. Dependencies — what BA5 needs to be useful

### 8.1 BA1 (citations) is a hard prerequisite

Critique axis 3.1 (unsupported claims) requires that the output's claims have citations to check. Without BA1's `ArtifactReference` discipline, the critique has nothing to verify against — every claim is "unsupported" by default, which collapses the axis to noise.

BA5 ships *with* BA1 or it doesn't ship. Recommend treating them as a coupled ratification.

### 8.2 BA6 (connector layer) is the source-of-truth substrate

Critique axis 3.1 also requires that cited sources are reachable for `content_hash` validation. That's what BA6's connector layer provides. Without BA6, citations are opaque strings; the critique can verify presence but not freshness or actual textual support.

BA5 ships *with* BA6's starter set (Linear, Confluence, Notion, Drive per the BA6 scoping doc) or its source-checking is degraded.

### 8.3 BA3 (traceability matrix) is required for axis 3.5

Requirements-coverage critique depends on the `RequirementRef` data structure. Without BA3, the auditor can flag "no requirements referenced" but cannot evaluate coverage substance.

### 8.4 A3 is the substrate

The model-call infrastructure, the separate-vendor selector, the proof-chain integration, the human-sampling escalation, the operator override — all of these are A3 patterns. BA5 reuses them. BA5 alone without A3 means re-implementing all of A3's infrastructure, which is wrong.

### 8.5 R5 must hold

If replay-mining-on-traces is allowed, the critique-pass signal becomes a training pressure: agents learn to produce outputs the critique can't object to, regardless of whether those outputs are truthful. Same dependency as A3 — BA5's safety guarantee assumes R5 is permanent.

---

## 9. Failure modes and what we do about them

### 9.1 The critique is itself wrong

Auditors are imperfect. The critique might block an artifact that's actually fine, or pass an artifact that has a problem. Mitigations:

- **Operator override** with audit (per §6 two-eyes rule).
- **Sampling**: 5% of *passed* artifacts are randomly routed to human spot-check; spot-check disagreements train the operator's intuition about auditor reliability.
- **Multi-auditor for catastrophic**: two distinct critique agents (different vendors) run in parallel; disagreement escalates.

### 9.2 The artifact is generated to pass the critique

If the BA-orchestrator's behavior is shaped (by training, prompt evolution, or replay mining) to produce artifacts the critique won't object to, the system has Goodhart'd. R5 quarantine prevents the training-side path; prompt evolution requires operator vigilance.

### 9.3 The critique is presented but ignored

A critique that's rendered but the human reader doesn't look at is functionally absent. Mitigations:

- Format the critique to require visible engagement (not collapsed by default, inline marginal notes).
- For catastrophic-class, require explicit human acknowledgment before delivery.
- Audit log records whether the critique was *shown* (presentation layer signal) — operator can review their own engagement post-hoc.

### 9.4 Sources are stale by the time the critique runs

If the connector layer (BA6) has cached source content that's now out of date, the `content_hash` check will catch the staleness but the critique itself may not know whether the staleness is meaningful (a punctuation change vs. a paragraph rewrite). Mitigations:

- Re-pull cited sources at critique time, within a configurable freshness window (e.g., 24h since retrieval; 1h for catastrophic-class).
- Flag stale-with-meaningful-change as a Warn-class finding; operator decides whether to rework.

### 9.5 The critique is too slow

Each artifact gains one auditor call. For high-volume internal artifacts (BA-routine class), this cost matters. Mitigations:

- Routine-class critique can be lighter (smaller model, fewer axes, async — the artifact ships and the critique attaches asynchronously).
- Substantial- and catastrophic-class critique is synchronous and blocking; the cost is the price of trust.
- Aggregate critique latency surfaced via sentinel metrics so the operator can see it.

### 9.6 The critique reveals proprietary or sensitive information

Critique findings may surface details about the source materials (e.g., "this claim about Q3 revenue doesn't match the cited Confluence page"). For multi-tenant deployments, this could leak across tenant boundaries.

- Critique storage respects the same tenant boundaries as the artifact itself.
- Sensitive findings (per BA6's redaction rules) are summarized rather than quoted.
- This is the same tenant-scoping concern called out in BA6 §6.4; same mitigation (capability tokens per ADR-018).

---

## 10. Concrete implementation skeleton

```rust
// sentinel-application/src/hooks/ba_critique.rs

pub fn process(
    input: &HookInput,
    fs: &dyn FileSystemPort,
    critic: &dyn BaCriticPort,
    Legatus AI: &dyn LegatusAiPort,
) -> HookOutput {
    // Step 1: Is this tool call producing a BA artifact destined for a human?
    let Some(artifact) = ba_artifact::extract(input) else {
        return HookOutput::allow();  // Not a BA output
    };

    // Step 2: Classify the artifact
    let class = ba_classification::classify(&artifact, fs);

    // Step 3: Critique
    let critique = critic.score(&artifact)?;

    // Step 4: Attach critique to artifact (becomes a sibling proof entry)
    proof::attach_critique(fs, &artifact, &critique)?;

    // Step 5: Decision tree
    match (class, &critique.overall_recommendation) {
        (_, CritiqueDisposition::Ship) => {
            HookOutput::allow_with_attachment(critique)
        }
        (BaArtifactClass::Routine, _) => {
            // Routine class: even Block-class findings only warn; operator reviews async
            HookOutput::allow_with_warning(critique)
        }
        (BaArtifactClass::Substantial, CritiqueDisposition::HumanReviewRequired) => {
            HookOutput::ask_user(format_critique_summary(&critique))
        }
        (BaArtifactClass::Catastrophic, _) => {
            // Two-eyes: always route to operator for explicit acknowledgment
            Legatus AI.request_operator_acknowledgment(&artifact, &critique)?;
            HookOutput::pending_human_acknowledgment()
        }
        (_, CritiqueDisposition::BlockForRework) => {
            HookOutput::deny(format_block_reason(&critique))
        }
    }
}
```

The shape is small. The critique-axis logic in `sentinel-domain/src/ba/critique.rs` is the substantive part.

---

## 11. Test strategy

- **Unit tests in `sentinel-domain/src/ba/`**: critique-axis scoring logic with mocked findings; classification rules with fixture artifacts.
- **`BaCriticPort` mock**: in-memory critic returning canned `BaCritique` results; hook tests cover Ship / HumanReviewRequired / BlockForRework dispositions for each artifact class.
- **Citation-presence test**: artifact with N citations / N claims; critic axis 3.1 score matches expected.
- **Stale-citation test**: artifact citing a `content_hash` that no longer matches the source; axis 3.1 produces Warn-class finding.
- **Tonal-spin test**: artifact with confident claims and weak citations; axis 3.4 produces Warn-class finding.
- **Two-eyes integration test**: catastrophic-class artifact + critique with Block finding → Legatus AI receives operator-acknowledgment request → operator accepts → action proceeds with audit entry showing the override.
- **Presentation-layer contract test**: assert that rendered artifact + critique are both present in output; assert that critique is not in a collapsed-by-default state.

---

## 12. Open questions

1. **Per-axis configurable thresholds.** Should the operator be able to tune "what citation density is enough" (axis 3.3) per workflow? Recommend yes; defaults documented; per-workflow overrides via `config/ba-classes.toml`.

2. **Multi-stakeholder critique.** Should an exec deck for the CFO use different critique axes (more emphasis on financial-claim verification) than an exec deck for the CEO (more emphasis on strategic-alignment claims)? Probably yes eventually; out of scope for v1 — uniform critique axes ship first.

3. **Critique-of-critique.** Should the operator be able to request a second critique from a different auditor when they suspect the first was wrong? Recommend yes; trivial to implement once `BaCriticPort` has multiple adapters; UX is a "Re-critique with different model" button.

4. **Critique caching.** If the artifact hasn't changed, can the critique be reused without re-running? Recommend yes for the artifact content_hash + critic model + auditor model triple; cache invalidation on any change.

5. **Voice-rendered critique structure.** When Legatus AI reads the critique aloud during a voice briefing, what's the format? Recommend: severity-sorted, Block first, then Warn, Info only on request. Out of scope for design doc; UX detail.

---

## 13. Decision and ownership

- **Decision class:** sentinel architectural change. Adds a new hook, a new port, a new value-object module, and a new adapter category in `sentinel-infrastructure/`.
- **Owner:** Gary Somerhalder ratifies. Co-requires A3 + BA1 + BA6 ratification.
- **Re-evaluation cadence:** revisit after first 100 BA-vertical artifacts have shipped under BA5 — calibrate critique-axis thresholds, prune false-positive Block findings, tune sampling rate.
- **Related items in the brief:** BA5 (this), A3 (substrate), BA1 (citation prerequisite), BA3 (traceability prerequisite), BA6 (connector source prerequisite), R8 (the retirement this complements), R5 (must hold for BA5's safety to hold).

---

## 14. Methodology caveat

This doc cites no external research not already covered in upstream docs. It applies A3's auditor pattern (cited there) to the BA-vertical context. The R8 retirement's evidence (Cognition critique, Greenblatt 2412.14093 alignment faking, Air Canada chatbot tribunal, legal-brief incidents) is the empirical basis for why BA5 matters; no new citations needed.

## 15. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building the `ba_critique` hook + `BaCriticPort` + adapter category.
- Treating BA1, BA3, BA6, A3, R5 as co-requirements (BA5's effectiveness depends on all five).
- The presentation discipline: critique and artifact rendered together, always.
- The two-eyes rule for catastrophic-class outputs.
