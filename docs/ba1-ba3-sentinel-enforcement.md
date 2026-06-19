# BA1 + BA3 — Sentinel-Side Enforcement

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendations **BA1** (citation-locked decision artifacts) and **BA3** (requirements traceability matrix), both S-tier
**Related:**
- `docs/ba6-connector-layer-scoping.md` — defines the connector layer that produces citable artifacts; flags the two new hooks specified here
- `docs/a3-dry-run-then-commit.md` — A3's auditor pattern is the substrate; BA5 reuses it for critique, BA1/BA3 reuse it for enforcement
- `docs/ba5-adversarial-deck-critique.md` — BA5 consumes BA1/BA3 enforcement signals as critique inputs (axes 3.1, 3.3, 3.5)
- `docs/policy-no-auto-summary-without-critique.md` (R8) — paired retirement that this enforcement implements
- Legatus AI ADR-017 (Artifact + Requirement Metadata Extensions) — defines the wire format (`ArtifactReference`, `RequirementRef`); this doc specifies the sentinel-side gate that enforces it
- Memory: `architecture-hexagonal-ddd`

---

## TL;DR

BA1 (every claim source-pinned) and BA3 (every recommendation traceable to a requirement) are the two S-tier disciplines that distinguish the AI-factory BA product from a generic LLM. Legatus AI ADR-017 specifies the wire format that lets dispatcher and legatus exchange artifact and requirement metadata. **This doc specifies the sentinel-side enforcement** — three new hooks that turn the wire format into a hard gate at the hook layer:

- **`audit_extract`** — lifts every connector call (per BA6) into sentinel's audit chain. Without this, citation provenance is unverifiable post-hoc.
- **`provenance_validate`** — gates BA-orchestrator outputs containing claims against the connector audit record. Every cited `artifact_id` must have a corresponding connector retrieval. Catches BA1 violations *structurally* rather than relying on agent discipline.
- **`requirements_traceability_gate`** — gates BA-orchestrator outputs containing recommendations against the `RequirementRef` chain. Every recommendation must trace to a stated requirement. Catches BA3 violations structurally.

All three are pure sentinel hooks; they do not require code changes in Legatus AI or in the BA-orchestrator. They read Legatus AI ADR-017's structured payloads (which flow through sentinel's existing PreToolUse / PostToolUse pipeline) and apply policy. `ObserveOnly` is the rollout posture for calibration; blocking modes fail closed for structural validation failures.

Together with BA5 (which uses the same signals as critique inputs) and A3 (which supplies the model-call infrastructure), these three hooks complete the BA-vertical's S-tier substrate.

---

## 1. The architectural problem

Legatus AI ADR-017 specifies that `RelayInstruction` and `InstructionResult` carry optional `artifacts: Vec<ArtifactReference>` and optional `requirement_refs: Vec<RequirementRef>` / `fulfilled_requirements: Vec<RequirementRef>`. The wire shape is right. But the wire format alone doesn't enforce that the fields are *populated correctly* — or at all.

A BA-orchestrator that ships an instruction without `artifacts` populated is a logic bug, not a wire violation. A BA-orchestrator that ships a `RequirementRef` that doesn't actually trace to a real stakeholder need is a content bug, not a wire violation. These are exactly the failures BA1 and BA3 are designed to make impossible — but only if something *checks*.

Without sentinel-side enforcement:
- BA1 is aspirational. The wire format is there; nothing forces use.
- BA3 is aspirational. Same.
- BA5 (adversarial deck critique) can flag missing citations and missing requirement refs as critique findings, but only *after* the output is generated. The failure has already happened; the critique just catches it.

With sentinel-side enforcement, BA1/BA3 violations are caught at the PreToolUse hook before the BA-orchestrator's tool call produces an output that would need to be retracted.

---

## 2. Three new hooks

### 2.1 `audit_extract` — connector audit into sentinel chain

**Trigger:** Every PostToolUse for a tool call matching `mcp__<connector>__*`, where the connector is registered as a documentation-source connector per BA6.

**Action:** Reads the connector's structured audit event (per BA6 §3.4) from the PostToolUse payload and emits a corresponding entry in sentinel's audit log + proof chain. Tags it with `connector_name`, `tool_name`, `consumer_identity_label`, `target_resource_summary`, `outcome`, `latency_ms`, `bytes_returned`.

**Why it's needed:** Provenance validation (`provenance_validate`, next hook) needs to know which connector calls actually happened in this session. Without this lift, the connector's own audit log is its own — sentinel can't reason about it. With this lift, sentinel's audit chain becomes the source-of-truth for "what was retrieved when."

**Hex/DDD layering:**
- `sentinel-application/src/hooks/audit_extract.rs` — the hook itself.
- Depends on the existing `mcp_health` hook's connector registry (or its own minimal registry of which MCP tools are documentation connectors per BA6's classification).
- Emits via existing `AuditSink` adapter (no new adapter needed).

**Failure modes:**
- Connector audit event malformed → audit_extract emits a `MalformedConnectorAudit` entry; the call itself is not blocked (sentinel doesn't have semantic authority over the connector's output).
- Audit storage failure → connector call proceeds; sentinel emits a health warning; the missing audit entry is reconstructible from connector's own log if needed.

### 2.2 `provenance_validate` — BA1 structural enforcement

**Trigger:** PreToolUse for any tool call that ships a BA-orchestrator output (the "publish" / "send" / "render" tools — defined per a `config/ba-outputs.toml` registry).

**Action:** Reads the output's `artifacts: Vec<ArtifactReference>` (per Legatus AI ADR-017). For each cited `artifact_id`, validates:
1. **Existence**: a corresponding entry exists in sentinel's audit chain from `audit_extract` (i.e., the connector was actually called for this artifact).
2. **Freshness**: the cited `content_hash` matches the latest hash in the audit chain (catches stale citations).
3. **Provenance class**: the cited `provenance_class` matches what the connector emitted (catches mis-classification — e.g., labeling an Inference as SystemOfRecord).
4. **Within session**: the connector call happened in this session (or within a configurable lookback window — default 24h).

**Decision:**
- All citations validate → allow.
- Any citation fails *Existence* check → block (Block-class finding). The agent is told which citation has no provenance.
- Citation fails *Freshness* → warn or block depending on configuration (recommend: block for catastrophic-class outputs, warn otherwise).
- Citation fails *Provenance class* → warn (the agent may have legitimately reclassified; surface to operator).
- Provenance store unavailable or malformed → block in blocking modes; citations cannot pass without validated history.

**Why it's needed:** BA1 is "every claim source-pinned." This hook makes BA1 *structurally enforceable* rather than agent-discipline-dependent. The agent cannot ship a deck with hallucinated citations because the hook catches the citation-without-provenance pattern at PreToolUse.

**Hex/DDD layering:**
- `sentinel-application/src/hooks/provenance_validate.rs`.
- New port `ProvenancePort` in `sentinel-domain/src/ports/` — pure trait, abstracts the audit-chain query.
- Adapter in `sentinel-infrastructure/src/provenance/` reads from the existing AuditSink storage.

**Failure modes:**
- Cited artifact was retrieved in a *prior* session and the lookback window has expired → the agent should re-retrieve. Default mode warns; strict mode blocks.
- Connector was unavailable at retrieval time and a stale cache was used → the cache should have been content-hashed too; staleness is detected via *Freshness* check.
- Provenance store unavailable or malformed → block in blocking modes; `ObserveOnly` is the only non-blocking calibration posture.
- Operator override available via `hygiene_override` for edge cases the operator deems valid.

### 2.3 `requirements_traceability_gate` — BA3 structural enforcement

**Trigger:** Same as `provenance_validate` — PreToolUse for BA-orchestrator publish/send/render tool calls.

**Action:** Reads the output's `requirement_refs: Vec<RequirementRef>` (per Legatus AI ADR-017). For each:
1. **Matrix presence**: the `matrix_row_id` exists in the active orchestration's requirements matrix (which is stored as part of the orchestration's state per Legatus AI ADR-017's `orchestration_id`).
2. **Requirement hash**: the `requirement_hash` matches the requirement's current hash in the matrix (catches outdated requirement references).
3. **Completion evidence**: if the output claims to *fulfill* a requirement (the output is an `InstructionResult` with `fulfilled_requirements` populated), the `completion_evidence` field must reference at least one artifact that *also* validates via `provenance_validate`.
4. **Coverage**: if the output is a recommendation, *at least one* `RequirementRef` must be present. A recommendation with no requirement traceback is the structural violation BA3 exists to prevent.

**Decision:**
- All requirement refs validate and coverage holds → allow.
- *Coverage* failure (recommendation with no requirement) → block (Block-class finding). Suggests the recommendation needs to be tied to a requirement or removed.
- *Matrix presence* failure → block. The requirement claimed doesn't exist in the matrix; either the matrix needs updating or the reference is wrong.
- *Hash* mismatch → warn (the requirement has changed since the agent referenced it — operator decides if the recommendation still applies).
- *Completion evidence* missing → warn or block depending on configuration.

**Why it's needed:** BA3 is "every recommendation traceable to a stated need." This hook makes "I made a recommendation without an upstream requirement" structurally impossible.

**Hex/DDD layering:**
- `sentinel-application/src/hooks/requirements_traceability_gate.rs`.
- New port `RequirementMatrixPort` in `sentinel-domain/src/ports/` — abstracts access to the orchestration's matrix state.
- Adapter in `sentinel-infrastructure/src/requirements/` — the matrix itself lives in the BA-orchestrator (per Legatus AI ADR-017 the orchestrator owns the matrix); the adapter is a read-only client.

**Failure modes:**
- Matrix not yet populated (early in an orchestration) → `ObserveOnly` during rollout, but `DefaultBlocking`/`StrictBlocking` fail closed once enforcement is enabled.
- Matrix lookup unavailable or malformed → Block-class finding in blocking modes; recommendations cannot pass on unvalidated requirement citations.
- Operator override available.

---

## 3. Enforcement modes — staged rollout

Recommend three rollout phases per hook:

1. **Observe-only** (initial deployment): the hook runs, logs findings, never blocks. Operator reviews findings to calibrate.
2. **Warn-mode**: the hook logs and surfaces findings to the operator via Legatus AI as runtime warnings, but still allows the action. Used while the BA-orchestrator is being trained/refined to populate citations and requirement refs reliably.
3. **Block-mode** (steady state): the hook fully enforces. Block-class findings prevent the action; operator override via `hygiene_override` is the escape valve for legitimate edge cases.

Each hook can be in a different mode (e.g., `provenance_validate` in Block-mode while `requirements_traceability_gate` is still in Warn-mode if the matrix infrastructure is less mature). Mode is configured per hook in `config/ba-enforcement.toml`:

```toml
[hooks.provenance_validate]
mode = "block"  # observe | warn | block
catastrophic_artifact_freshness_window_hours = 1
default_freshness_window_hours = 24

[hooks.requirements_traceability_gate]
mode = "warn"  # not yet block until matrix is reliable
soft_warn_when_matrix_empty = true
```

---

## 4. Interaction with BA5

BA5 (adversarial deck critique) reads the same signals these three hooks produce but at a different stage:

- **BA1/BA3 enforcement** (this doc) fires at *PreToolUse* on publish/send/render. Catches structural failures before they ship.
- **BA5 critique** fires at the artifact level (separate hook trigger). Catches semantic failures — "this claim's citation exists but the claim doesn't actually follow from the citation."

The two are complementary. Enforcement catches "no citation"; critique catches "citation doesn't support the claim." Enforcement catches "no requirement reference"; critique catches "requirement reference is technically there but the recommendation isn't really addressing it."

A BA-orchestrator output that passes both BA1/BA3 enforcement *and* BA5 critique is the highest-confidence BA output the system can produce.

---

## 5. Interaction with Legatus AI side ADR-017

Legatus AI ADR-017 is the wire-format spec; this doc is the enforcement spec. The relationship is one-way:

- **ADR-017 (Legatus AI)**: defines `ArtifactReference`, `RequirementRef`, optional fields on `RelayInstruction` / `InstructionResult`, `RequestBriefing` / `BriefingResponse`. The shape of the data on the wire.
- **This doc (sentinel)**: defines how sentinel reads that shape, audits it, and gates on it. The behavior at the sentinel hook boundary.

If Legatus AI ADR-017 doesn't ratify, sentinel's hooks will read empty `Vec<ArtifactReference>` from legacy messages and gate accordingly. In the shipped production baseline, that blocks BA output without citations instead of logging a telemetry-only warning, which is correct behavior even if the wire format is not being populated.

The two ADRs can ratify independently. Implementation order recommended: ADR-017 first (so the wire format is available); enforcement hooks second (so they have data to work with).

---

## 6. Interaction with A3 and the auditor seat

The A3 auditor (`AuditorPort`) is one model-call infrastructure. The BA5 critic (`BaCriticPort`) is another. Both live in `sentinel-infrastructure`, both honor the same separate-model-family vendor selection.

The three hooks in this doc are *not* model-call hooks. They are deterministic validators reading structured data. They don't need an auditor model; they need the data to validate against (the audit chain + the matrix). They are cheaper, faster, and more deterministic than A3 or BA5 — which is why they fire first.

The full pipeline for a BA-output publish:

```
BA-orchestrator tool call (e.g. mcp__notion__create_page with deck contents)
    ↓
PreToolUse hook chain:
    ├─ tool_usage_gate (existing) — basic discipline (task, plan-mode, sequential-thinking marker)
    ├─ provenance_validate (new, this doc) — BA1 structural check
    ├─ requirements_traceability_gate (new, this doc) — BA3 structural check
    ├─ ba_critique (new, BA5) — adversarial semantic critique
    └─ dry_run_then_commit (new, A3) — separate-family auditor for catastrophic-class
    ↓
Tool call executes; output reaches reader
    ↓
PostToolUse hook chain:
    └─ audit_extract (new, this doc) — lifts connector call audit if applicable
```

Each hook can independently allow, warn, ask-user, or deny. The order is: cheap deterministic checks first, expensive model-call checks last. A failure at any layer prevents subsequent layers from firing.

---

## 7. What this doc does NOT specify

- **The connector classification logic** (which MCP tools are "documentation source connectors" per BA6). That's BA6's scope.
- **The publish/send/render tool registry** (which tool calls count as "BA-orchestrator output destined for a human"). Needs its own small registry — `config/ba-outputs.toml`. Operator-extensible. Out of scope for this design doc; specified as an implementation detail.
- **The matrix-state storage**. The orchestration owns the matrix per Legatus AI ADR-017; sentinel reads via the `RequirementMatrixPort` adapter. The storage mechanism (in-memory, SQLite, etc.) is an adapter detail.
- **Specific block-class threshold tuning**. Initial defaults documented; operator-configurable; calibration happens after first 100 BA outputs.

---

## 8. Failure modes

### 8.1 BA-orchestrator hasn't populated the fields yet (early adoption)

Solution: stage rollout through `observe → warn → block` modes per hook. Don't go straight to block; let the orchestrator catch up.

### 8.2 Audit chain query is slow

`provenance_validate` queries the audit chain for every cited artifact. For an artifact-heavy deck (10+ citations), this could add measurable latency.

Solution: maintain a session-scoped artifact-retrieval index in memory; the query is O(1) for the common case (artifact retrieved in current session). Fall back to full audit query only for cross-session lookback.

### 8.3 Matrix lookup unavailable

`requirements_traceability_gate` needs the orchestration's matrix. If the orchestration has not published a readable matrix snapshot, the gate cannot validate cited requirements.

Solution: blocking modes fail closed with `MatrixUnavailable` or `MatrixMalformed`. `ObserveOnly` remains the rollout posture while matrix publication is being brought online; production blocking modes do not downgrade to permissive behavior.

### 8.4 Operator legitimately wants to ship without citations

Edge cases exist — internal-only quick-notes that aren't worth the discipline overhead. The `hygiene_override` pattern is the escape valve; operator marks "this output is non-BA-routine; skip BA1/BA3" and the audit log records the override.

The override is per-output, not per-session, to prevent broad bypasses.

### 8.5 Block-class finding on a critical-deadline catastrophic output

The gate blocks; the deadline approaches. Operator can override via `hygiene_override`. The override is audited. Post-incident review can examine *why* the override was used and whether the operator's judgment was right.

Recommendation: track override frequency over time; high override rates on the same hook mean the hook is calibrated wrong.

---

## 9. Hex / DDD layering

- **`sentinel-domain/src/ba/provenance.rs`**: `ProvenanceCheck` value object; `ProvenanceFinding` enum (Existence/Freshness/ProvenanceClass/WithinSession variants). Pure data.
- **`sentinel-domain/src/ba/requirements.rs`**: `RequirementCheck` value object; `RequirementFinding` enum. Pure data.
- **`sentinel-domain/src/ports/provenance.rs`**: `ProvenancePort` trait — `query_artifact_history(artifact_id) → Result<Vec<RetrievalRecord>, _>`. Pure trait.
- **`sentinel-domain/src/ports/requirement_matrix.rs`**: `RequirementMatrixPort` trait — `query_requirement(orchestration_id, matrix_row_id) → Result<Option<Requirement>, _>`. Pure trait.
- **`sentinel-application/src/hooks/audit_extract.rs`**: PostToolUse hook; reads connector audit from tool output; emits via existing `AuditSink`.
- **`sentinel-application/src/hooks/provenance_validate.rs`**: PreToolUse hook; reads `ProvenancePort`; decides allow/warn/block.
- **`sentinel-application/src/hooks/requirements_traceability_gate.rs`**: PreToolUse hook; reads `RequirementMatrixPort`; decides allow/warn/block.
- **`sentinel-infrastructure/src/provenance/`**: `ProvenancePort` adapter against the audit storage.
- **`sentinel-infrastructure/src/requirement_matrix/`**: `RequirementMatrixPort` adapter against the BA-orchestrator's matrix endpoint.
- **`config/ba-enforcement.toml`**: per-hook mode + threshold config.
- **`config/ba-outputs.toml`**: registry of which tool calls are BA-orchestrator publish/send/render.

All hex/DDD-respecting. New ports are pure traits in `sentinel-domain`. All IO confined to `sentinel-infrastructure`. All in-memory mocks available for tests.

---

## 10. Test strategy

- **Unit tests in `sentinel-domain/src/ba/`**: provenance + requirement check logic with fixture data.
- **`audit_extract` hook tests**: well-formed connector audit → lifted correctly; malformed → MalformedConnectorAudit emitted; storage failure → health warning.
- **`provenance_validate` hook tests**: artifact-with-valid-provenance → allow; artifact-with-no-provenance → block; stale citation → warn (or block in catastrophic mode); provenance store unavailable/malformed → block; operator override → allow with audit.
- **`requirements_traceability_gate` hook tests**: recommendation-with-traceable-requirement → allow; recommendation-without-requirement → block (coverage failure); requirement-hash-mismatch → warn; matrix unavailable/malformed → block.
- **End-to-end test**: simulated BA-orchestrator publish call; all three hooks fire in correct order; correct decision emerges.
- **Performance test**: artifact-heavy output (50 citations) measured for total gate latency; assert p95 under threshold (default 500ms).

---

## 11. Open questions

1. **Cross-session lookback window**. Default 24h for citation freshness. Should catastrophic-class outputs have a tighter window (e.g., 1h)? Recommend yes; documented above; configurable.

2. **Per-tenant matrix isolation**. If multiple BA-orchestrators share a sentinel instance (multi-tenant), can they see each other's matrices? Same answer as BA6 §6.4 and BA5 §9.6: capability-token-scoped (Legatus AI ADR-018 territory).

3. **Audit chain pruning vs. lookback**. The audit chain grows unbounded. At what point are old entries pruned, and does that affect cross-session lookback? Recommend: don't prune entries needed for the configured lookback window; prune older entries to cold storage with on-demand query.

4. **Connector audit reliability**. What if a connector lies about its audit event (compromised or buggy)? sentinel's audit_extract has no way to verify the connector's output is honest. Mitigations are out of scope for this doc (capability tokens for connector identity per ADR-018; connector integrity monitoring per BA6 §6.5).

5. **Soft-warn vs warn vs block escalation**. Current design has Block / Warn / Info. Should there be a Soft-Warn (logged-but-not-surfaced) tier? Recommend no — too much complexity for marginal value; mode-per-hook is sufficient granularity.

---

## 12. Decision and ownership

- **Decision class:** sentinel architectural change. Adds three hooks, two ports, two adapter directories, two config files.
- **Owner:** Gary Somerhalder ratifies. Co-requires Legatus AI ADR-017 ratification (wire format) and BA6 ratification (connector layer). Compatible with BA5 (which consumes these signals) and A3 (which runs in the same hook pipeline).
- **Re-evaluation cadence:** revisit after 100 BA outputs have flowed through the gates — calibrate thresholds, prune false-positive Block findings, evaluate operator override frequency.
- **Related items in the brief:** BA1 (this), BA3 (this), BA5 (downstream consumer), BA6 (upstream connector layer), A3 (parallel hook in the same pipeline), R8 (the retirement these hooks structurally enforce), R5 (must hold for the enforcement signal to not become a Goodhart target).

---

## 13. Methodology caveat

This doc relies on the upstream brief's evidence base for BA1 and BA3 (already cited there); no new external citations introduced. The Legatus AI ADR-017 design has its own caveats inherited from the brief.

## 14. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building three new hooks (`audit_extract`, `provenance_validate`, `requirements_traceability_gate`).
- Building two new ports (`ProvenancePort`, `RequirementMatrixPort`) and their adapters.
- The staged rollout mode (observe → warn → block) per hook.
- Treating Legatus AI ADR-017, BA6, BA5, A3, R5, R8 as the surrounding context that gives these hooks their meaning.
