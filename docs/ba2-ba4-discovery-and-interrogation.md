# BA2 + BA4 — Two-Mode Discovery and Stakeholder Interrogation Protocol

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendations **BA2** (two-mode discovery as a first-class primitive) and **BA4** (stakeholder interrogation protocol), both A-tier in the BA vertical
**Related:**
- `docs/ba6-connector-layer-scoping.md` (BA6) — provides the *automated* discovery substrate (system pulls via MCP connectors); BA2's automated mode IS BA6 in operation
- `docs/a13-spec-challenge.md` (A13) — A13's orchestration-start challenge produces the initial gap list; BA2/BA4 expand each gap into either automated or interrogated discovery work
- `docs/ba1-ba3-sentinel-enforcement.md` — interrogation answers become `ArtifactReference`s with `ProvenanceClass::Interview`; same enforcement rules apply
- `docs/ba5-adversarial-deck-critique.md` — BA5 reads discovery completeness (was a gap closed by automated retrieval, interrogated answer, or only inference?) when scoring artifacts
- Legatus AI ADR-016 (Legatus AI Peer Registration) + ADR-017 (Artifact + Requirement Metadata Extensions) — interrogation messages flow through Legatus AI to the human; the protocol respects the human-commander framing
- Memory: `architecture-hexagonal-ddd`

---

## TL;DR

BA2 makes **discovery — the act of acquiring the data a BA recommendation needs — a first-class primitive** with two named modes:

- **Automated discovery** (the BA2-A half): scheduled and on-demand pulls from MCP-connected systems of record (per BA6). Already well-trodden; this doc just names it and slots it into the discovery ledger.
- **Interrogated discovery** (the BA2-I / BA4 half): structured Q&A with stakeholders, routed through Legatus AI, batched by stakeholder, scheduled around availability, follow-up-aware, escalation-routed when answers don't arrive. **This is the novel half.** No current agent system treats interrogation as a primitive; agents either silently infer or write free-form chat — neither produces auditable provenance.

Both modes write to a **shared discovery ledger** that the BA-orchestrator queries to know: what data does the orchestration need (from A13's gaps), what has it acquired, what is it still missing, what has been asked but not answered. The ledger drives the orchestration's next-step decision.

The discipline this enables: every BA recommendation traces (via BA3's requirements traceability matrix) back through the artifacts that informed it, and every artifact is either a system-of-record pull or an interview-answered question — never an inference passed off as data. Inferences are still allowed, but they're explicitly typed as `ProvenanceClass::Inference` in the artifact reference, and downstream BA5 critique catches them as such.

---

## 1. Why BA2/BA4 is A-tier

The brief's framing:

> Real BAs don't just read documents; they ask. Agents that only do automated discovery produce hallucinated requirements because they fill the gaps where humans would have asked.

Today's BA-tooling market (Tableau, Looker, ThoughtSpot, Hex, etc.) is overwhelmingly automated-discovery — all connectors, no interrogation primitive. Agents built on top of this market inherit the gap: when they need a piece of information that isn't in a system of record, they invent it. That invention propagates through downstream artifacts, gets cited as if it were source-of-record, and reaches an exec deck as a confident-sounding fact.

The structural fix is: make the act of *not having* the information visible, and route it to a stakeholder who can answer. That's interrogated discovery. Without it, BA1 (citations) can be satisfied by inventing the source; with it, BA1 + BA2/BA4 together make hallucinated requirements structurally impossible — the gap can't be papered over silently; it has to be either asked-and-answered or explicitly typed as inference.

---

## 2. The discovery ledger

Both modes write to a single aggregate. The ledger lives in `sentinel-domain` (pure data) and is persisted via a `DiscoveryLedgerStorePort` adapter.

```rust
// In sentinel-domain/src/ba/discovery.rs (new module)
pub struct DiscoveryLedger {
    pub orchestration_id: OrchestrationId,
    pub entries: Vec<DiscoveryEntry>,
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}

pub struct DiscoveryEntry {
    pub entry_id: DiscoveryEntryId,
    pub spec_gap_ref: Option<SpecGapRef>,    // links to a gap from A13's SpecChallenge
    pub topic: String,                        // human-readable: "what is the current MRR breakdown by tier?"
    pub mode: DiscoveryMode,                  // Automated | Interrogated | Inferred
    pub status: DiscoveryStatus,              // Pending | InProgress | Answered | Abandoned | Escalated
    pub responses: Vec<DiscoveryResponse>,    // 0+ responses; latest wins
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}

pub enum DiscoveryMode {
    Automated {
        connector: String,                    // e.g., "confluence", "linear"
        query: String,
        last_attempt: Option<DateTime<Utc>>,
    },
    Interrogated {
        stakeholder_ref: StakeholderRef,
        question: InterrogationQuestion,
        scheduled_for: Option<DateTime<Utc>>, // respect availability windows
    },
    Inferred {
        basis: String,                        // why inference is acceptable for this entry
        confidence: InferenceConfidence,
    },
}

pub enum DiscoveryStatus {
    Pending,                                  // entry created; mode chosen but not yet acted on
    InProgress,                               // automated query running OR interrogation question sent
    Answered { artifact_ref: ArtifactReference },  // satisfied; downstream may cite the artifact
    Abandoned { reason: String },             // gap is explicitly not going to be closed
    Escalated { to: StakeholderRef, escalation_reason: String },  // stakeholder didn't answer; routed up
}

pub struct DiscoveryResponse {
    pub responded_at: DateTime<Utc>,
    pub raw_content: String,
    pub responder: ResponderRef,              // who/what produced this response
    pub artifact_ref: Option<ArtifactReference>, // for Answered status
}
```

The ledger is **per-orchestration** (one ledger per `OrchestrationId`). Multiple BA-orchestrations have independent ledgers. The aggregate is updated by the discovery-coordinator hook (BA2/BA4 hook) as discovery work proceeds.

---

## 3. Automated mode — BA2-A

Triggered by an entry with `DiscoveryMode::Automated { connector, query, .. }`. Flow:

1. The discovery-coordinator hook fires the connector call via MCP (using A2's `CapabilityRouterPort` to pick the right connector — BA6 territory).
2. The call's audit event is lifted by `audit_extract` (per BA1+BA3 enforcement doc).
3. The response artifact is checked via `provenance_validate` (BA1) and attached to the discovery entry's `responses`.
4. Status moves to `Answered` with the `artifact_ref`.

Automated mode is **synchronous by default** (the orchestration awaits the response) and **schedulable as a fallback** for connectors with long-running queries (e.g., a large Drive search). Schedule-and-poll uses sentinel's existing cron infrastructure.

Failure modes for automated:
- Connector unavailable → status `Escalated { reason: ConnectorOutage }`; entry becomes a candidate for interrogation as a fallback.
- Empty result → status stays `Pending`; orchestrator decides whether to broaden the query, escalate to interrogation, or accept the gap.
- Connector returned data that fails freshness check (`content_hash` mismatch with prior cite) → response attached but flagged via BA1's `provenance_validate`; downstream consumers see the staleness.

---

## 4. Interrogated mode — BA2-I / BA4

This is the novel half. The protocol details:

### 4.1 The InterrogationQuestion

```rust
pub struct InterrogationQuestion {
    pub question_id: QuestionId,
    pub text: String,                         // the actual question to the stakeholder
    pub structured_intent: QuestionIntent,    // why we're asking — for stakeholder clarity
    pub answer_shape: AnswerShape,            // FreeText | Numeric | Enum(...) | Boolean | Date
    pub blocking: bool,                       // does the orchestration block on this answer?
    pub urgency: Urgency,                     // Low | Standard | High | Catastrophic-blocked
    pub timeout: Option<Duration>,            // when to escalate if no answer
    pub follow_up_chain: Vec<QuestionId>,     // questions only asked if this one is answered
}

pub enum QuestionIntent {
    DefineRequirement { requirement_topic: String },
    DisambiguateInterpretation { interpretations: Vec<String> },
    ConfirmAssumption { assumption_text: String },
    VerifyInferenceAcceptable { inferred_value: String, basis: String },
    PrioritizeTradeOff { options: Vec<String> },
}
```

The structured intent matters: a stakeholder reading "what is the current MRR by tier?" responds differently when they understand whether the question is "to define a requirement" vs "to confirm an assumption already made." Stakeholders are also more willing to engage with structured questions than with free-form chat from an agent.

### 4.2 Batching by stakeholder

Stakeholders dislike being interrogated piecemeal. The discovery-coordinator hook **batches** questions per stakeholder:

- Per-stakeholder queue per orchestration.
- Batches flush on a schedule (e.g., once per business day) OR when accumulated questions reach a threshold (e.g., 5 questions OR any one is `Catastrophic-blocked`).
- The batch is delivered as a single structured message via Legatus AI (per Legatus AI's voice/text supervision channel — the operator's Legatus AI session) to the stakeholder, NOT a chat avalanche.

The operator can configure per-stakeholder batching policies in `config/ba-interrogation.toml`:

```toml
[[stakeholders]]
ref = "ceo"
display_name = "Jane Doe (CEO)"
channels = ["email", "operator-relay"]
batching = { mode = "scheduled", schedule = "0 9 * * 1-5" }  # M-F 9am
max_batch_size = 3
preferred_response_window = "24h"

[[stakeholders]]
ref = "head-of-sales"
display_name = "Bob (Head of Sales)"
channels = ["operator-relay"]
batching = { mode = "threshold", threshold = 5 }
preferred_response_window = "1h"
```

### 4.3 Routing via Legatus AI

Interrogation messages flow through Legatus AI (per Legatus AI ADR-016's `HumanOperator` Legatus AI peer identity). The BA-orchestrator (an `AutomatedOrchestrator` peer per ADR-016) emits an interrogation request to Legatus AI; Legatus AI routes the request via the human-operator's voice/text supervision channel; the operator relays to the stakeholder (or the system stakeholder for some channels).

This respects the architecture's human-commander framing: the BA-orchestrator never speaks directly to stakeholders; the operator's legatus-ai-app is the channel. The operator can:
- Forward the batch directly (most common).
- Edit before forwarding (clarifying or rephrasing).
- Reject (this question shouldn't be asked).
- Hold (defer for context-specific reasons).

### 4.4 Follow-up chains

Questions have follow-ups conditional on the answer. Example:

```
Q1: "Are you planning to enter the European market in 2026?"
  → if "no": skip Q2, Q3
  → if "yes": Q2: "Which countries first?"
                → Q3: "What's the budget for the EU expansion?"
```

The discovery-coordinator hook receives the answer and triggers conditional follow-ups, which join the next batch (subject to the same batching policy).

### 4.5 Timeouts and escalation

Each question has an optional `timeout`. When the timeout elapses without an answer:

- **Low urgency**: question marked `Abandoned { reason: NoResponseWithinTimeout }`; orchestrator proceeds with explicit `Inferred` for the underlying gap (BA5 critique will flag the inference downstream).
- **Standard urgency**: question marked `Escalated { to: stakeholder_manager_or_alternate }`; same batching applies for the escalation target.
- **High urgency** or **Catastrophic-blocked**: orchestration pauses, surfaces via Legatus AI to the operator with explicit "this question is blocking work; the stakeholder hasn't answered in T time."

Escalation paths are operator-configured in `config/ba-interrogation.toml`'s stakeholder hierarchy.

---

## 5. Interaction with A13

A13's orchestration-start challenge produces the initial list of gaps. Each gap becomes one or more `DiscoveryEntry`s:

```
A13 SpecChallenge.gaps[i]
  ↓ (BA2/BA4 coordinator decides discovery mode)
DiscoveryEntry {
    spec_gap_ref: Some(SpecGapRef(challenge_id, i)),
    topic: "...",
    mode: Automated { connector: "linear", query: "..." } | Interrogated { ... }
}
```

The discovery-coordinator hook's *mode-selection logic* is deterministic per the gap's `how_resolved` value:

- `GapResolution::OperatorClarified` → typically `Interrogated` (operator answered already, but the canonical pattern is the operator asks the stakeholder).
- `GapResolution::InferredFromContext` → typically `Inferred` with high confidence; sometimes promoted to `Interrogated` if the inference is risky.
- `GapResolution::DefaultApplied` → typically `Inferred` with low confidence; usually promoted to `Interrogated` for irreversible+ class work.

The orchestrator can override the default per-gap.

---

## 6. Interaction with BA1 and BA3

### 6.1 BA1 — Interview answers as ArtifactReferences

When an interrogation receives an answer, the answer becomes an `ArtifactReference` with `ProvenanceClass::Interview`:

```rust
ArtifactReference {
    artifact_id: ArtifactId(format!("interview://{}/{}", orchestration_id, question_id)),
    artifact_type: ArtifactType::Other,
    source_uri: format!("interview-channel://{}", channel_used),
    content_hash: hash_of(answer_text),
    retrieved_at: response_timestamp,
    provenance_class: ProvenanceClass::Interview,
}
```

Downstream BA1 enforcement treats interview citations as first-class — they pass `provenance_validate` because the interrogation flow produced a corresponding audit record at the time of the answer.

### 6.2 BA3 — Requirements traceability through discovery

When discovery answers map to requirements (the common case for `QuestionIntent::DefineRequirement`), the requirement's `completion_evidence` includes the interview's `ArtifactReference`. BA3's requirements-traceability gate validates the chain end-to-end:

`stakeholder need → SpecGap → InterrogationQuestion → DiscoveryEntry::Answered → ArtifactReference → RequirementRef → recommendation`

The chain is auditable. Every recommendation can be traced back to either an interview answer or a system-of-record pull or an explicit inference, with the audit chain showing each step.

---

## 7. Hex / DDD layering

- **`sentinel-domain/src/ba/discovery.rs`** (new): `DiscoveryLedger`, `DiscoveryEntry`, `DiscoveryEntryId`, `DiscoveryMode`, `DiscoveryStatus`, `DiscoveryResponse`, `InterrogationQuestion`, `QuestionId`, `QuestionIntent`, `AnswerShape`, `Urgency`, `StakeholderRef`, `ResponderRef`, `InferenceConfidence`. Pure data.
- **`sentinel-domain/src/ports/discovery_ledger_store.rs`** (new): `DiscoveryLedgerStorePort` trait — load/save/query the ledger. Pure trait.
- **`sentinel-domain/src/ports/interrogation_channel.rs`** (new): `InterrogationChannelPort` trait — `dispatch_batch(stakeholder, batch) -> Result<...>`. Pure trait. The adapter is the Legatus AI connection.
- **`sentinel-application/src/hooks/ba_discovery_coordinator.rs`** (new): the hook. Runs as a PostToolUse on A13's challenge emission (to seed entries from gaps), as a periodic batched dispatch (per stakeholder cron), and as a PreToolUse on BA-orchestrator's "I want to use data X" tool calls (to check if X is in the ledger).
- **`sentinel-infrastructure/src/discovery/`** (new adapter dir): `ledger_store/` (SQLite/JSONL adapter for the ledger), `interrogation_channel/` (Legatus AI adapter that wraps Legatus AI ADR-016/017 messages).
- **`config/ba-interrogation.toml`** (new, operator-managed): stakeholder registry + batching policies + escalation hierarchy.

All hex/DDD-respecting per `[[architecture-hexagonal-ddd]]`. Pure value objects in domain. Two new ports. Adapters in infrastructure (one queries Legatus AI, one is a local store). In-memory adapters for tests.

---

## 8. Failure modes

### 8.1 Stakeholders ignore the interrogation

Mitigations: configurable timeouts; escalation chains in `config/ba-interrogation.toml`; high-urgency questions surface to operator if unanswered; orchestration proceeds with explicit `Inferred` flagging (and downstream BA5 catches the inference).

### 8.2 Stakeholders give noisy or contradictory answers

The discovery-coordinator hook does not arbitrate semantic quality — answers are stored as-is. BA5's adversarial critique catches contradictions when synthesizing the output. If the critique finds the stakeholder's answer is contradicted by an automated source, that's a Warn-class finding for human review.

### 8.3 Orchestrator over-asks

Stakeholder fatigue is real. Mitigations: per-stakeholder batch size caps in config; the discovery-coordinator hook tracks asks-per-week per stakeholder and emits a health warning when thresholds exceed (default: more than 10 questions per week per stakeholder per orchestration triggers warning).

### 8.4 The operator can't always be the channel

For some workflows the operator isn't the right intermediary (e.g., a stakeholder the operator has no relationship with). The `channels` array per stakeholder in config allows direct channels (email, Slack DM) — but every direct-channel message *still* goes through Legatus AI's audit. The operator sees the dispatch even if they aren't relaying.

### 8.5 Interrogation crosses tenant boundaries

For multi-tenant deployments, interrogation must not leak one tenant's questions to another tenant's stakeholders. Same tenant-scoping concern as BA1, BA5, BA6, A3 — capability tokens (Legatus AI ADR-018) provide the boundary; Phase 1 (sandbox) is single-tenant only.

### 8.6 Inference quietly proliferates

If the orchestrator defaults to `Inferred` mode too readily, the ledger fills with inferences that look like data. Mitigation: per-orchestration metric on the inference-vs-answered ratio; high inference ratios trigger an operator health warning ("this orchestration is inferring most of its data; consider whether the work warrants interrogation").

---

## 9. Test strategy

- **Unit tests in `sentinel-domain/src/ba/discovery.rs`**: ledger aggregate operations; entry status transitions; follow-up chain expansion.
- **DiscoveryLedgerStorePort mock**: in-memory store; CRUD + query.
- **InterrogationChannelPort mock**: in-memory dispatch with canned responses; batching logic verified.
- **Integration with A13**: A13 emits SpecChallenge with N gaps; discovery-coordinator hook seeds N DiscoveryEntry; modes assigned per default rules; verified.
- **Integration with BA1**: interview answer arrives; ArtifactReference produced with ProvenanceClass::Interview; provenance_validate accepts.
- **Integration with BA3**: completion_evidence on RequirementRef references interview artifacts; requirements_traceability_gate validates the chain.
- **Batching test**: 3 questions for same stakeholder, threshold=5: no dispatch; 5 questions or threshold-met (Catastrophic-blocked): dispatch.
- **Escalation test**: timeout elapses with no answer; status moves to Escalated; new batch built for escalation target.
- **Inference-ratio metric test**: orchestration with high inference ratio triggers health warning.

---

## 10. Open questions

1. **Should the stakeholder registry support free-form metadata?** (E.g., "this stakeholder prefers concrete examples in questions"). Recommend yes; `metadata: HashMap<String, String>` field on the stakeholder; prompt templates can query it for question tone.

2. **What about anonymous answers?** (E.g., survey-style responses where the stakeholder identity is intentionally not in the audit trail.) Out of scope for v1; the protocol assumes identified stakeholders. A future ADR can add anonymity-supporting modes.

3. **Real-time interrogation for voice Legatus AI sessions?** Today the model is batched + scheduled. Could the operator's legatus-ai-app surface "the orchestrator is asking X; want me to ask the stakeholder now?" in voice? Recommend yes as a v2; v1 ships batched-async only.

4. **Stakeholder-side authoring of structured answers.** Today the operator relays free-text and translates to the structured `AnswerShape`. Could the stakeholder fill in a structured form directly? Possible for some channels (email with structured templates); flagged for future work.

5. **Discovery-graph visualization.** The ledger forms a graph (gaps → entries → artifacts → requirements → recommendations). An operator-facing report rendering this would be valuable. Out of scope for this design doc; flagged.

---

## 11. Decision and ownership

- **Decision class:** sentinel architectural change. Adds a value-object module, two ports, a hook, an adapter directory, and one new config file.
- **Owner:** Gary Somerhalder ratifies. Co-requires A13 (the challenge seeds the ledger), BA6 (the automated mode), BA1+BA3 enforcement (the audit + traceability of answers), Legatus AI ADR-016+017 (the interrogation transport).
- **Re-evaluation cadence:** revisit after first 100 interrogations across N orchestrations — calibrate batching defaults, escalation thresholds, inference-ratio warning level.
- **Related items in the brief:** BA2 (this — automated half), BA4 (this — interrogated half), A13 (substrate), BA6 (automated substrate), BA1 (citation enforcement), BA3 (traceability), BA5 (consumer — inference-ratio feeds critique).

---

## 12. Methodology caveat

This doc cites no external research not already covered upstream. The interrogation-as-primitive framing is novel relative to today's BA-tooling market; the closest analog is the requirements-engineering literature (Sommerville, Wiegers) where structured stakeholder elicitation is the canonical pattern. No specific ArXiv claims to verify.

## 13. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building `DiscoveryLedger` + `DiscoveryLedgerStorePort` + `InterrogationChannelPort` and adapters.
- Building the `ba_discovery_coordinator` hook.
- Shipping `config/ba-interrogation.toml` with the documented stakeholder schema.
- Routing interrogation through Legatus AI peers (per ADR-016/017).
- Treating A13, BA6, BA1+BA3 enforcement as co-requirements.
