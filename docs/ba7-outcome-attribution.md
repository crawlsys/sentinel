# BA7 — Outcome Attribution for Recommendations

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **BA7** (A-tier; closes the artifact-to-business-metric loop)
**Related:**
- `docs/ba1-ba3-sentinel-enforcement.md` — BA3 traceability matrix is the structural prerequisite; without it, there's nothing to attribute outcomes *to*
- `docs/ba2-ba4-discovery-and-interrogation.md` — discovery answers feed into the requirement chain BA7 measures against
- `docs/ba5-adversarial-deck-critique.md` — BA5 critiques *artifact quality*; BA7 measures *real-world outcome*. Together they distinguish "looked good" from "actually worked"
- `docs/ba6-connector-layer-scoping.md` — connector layer feeds outcome metrics back the same way it feeds source data (the metric *is* a source pull)
- `docs/policy-replay-mining-quarantine.md` (R5) — load-bearing boundary: outcome data informs the operator dashboard; it must not become a training signal
- `docs/policy-no-outcome-only-evaluation.md` (R14) — BA7 *adds* outcome to the picture; does NOT replace process supervision
- Memory: `architecture-hexagonal-ddd`

---

## TL;DR

A BA's actual value is **decisions changed and metrics moved**. Process quality (citations, traceability, critique) is the means; outcomes are the end. BA7 introduces an **`OutcomeRecord` aggregate** that follows each delivered recommendation through a four-stage lifecycle:

1. **Delivered** — artifact reached its reader (timestamped, audited).
2. **Acted-on** — decision-maker took the recommended action (or didn't).
3. **Metric-moved** — the metric the recommendation targeted moved (or didn't, or moved opposite).
4. **Survived-scrutiny** — the recommendation survived the next quarter's review (or got reversed).

Attribution is hard. The brief is explicit: partial measurement is much better than zero. BA7 ships with proxy metrics where causal attribution is impossible (e.g., "the PR landed and stayed green for 30 days" is a proxy for "the recommendation worked"; not perfect, much better than "the PR landed"). Operators can extend metrics per workflow via `config/ba-outcomes.toml`.

Critical boundary (R5): outcome data is for **operator-facing dashboards and post-hoc reasoning**, not for training agents to produce recommendations that "look outcome-positive." Same boundary as the appraisal counters in A2 — dispatch / measurement is fine; training-on-traces is not.

---

## 1. Why BA7 is A-tier

The brief's framing:

> A BA's actual value is decisions changed and metrics moved. Process artifacts are means, not ends. Without outcome attribution, the BA system optimizes for confident-sounding artifacts rather than impact.

Without BA7, the AI factory's BA-vertical product is structurally evaluable only on *process quality* (BA1 citations clean, BA3 traceability complete, BA5 critique passed). All three can be perfect on a recommendation that turned out to be useless or wrong. The system has no way to learn (in the operator's head, not the agent's training data) which kinds of recommendations actually move metrics.

The fix is not "score every recommendation by impact." Real attribution is hard. The fix is "track what we *can* observe — was the recommendation acted on? did the targeted metric move? did the decision get reversed later? — and surface that to the operator so the operator can adjust the workflow."

BA7 is the A-tier outcome anchor that complements:
- **A12** (external benchmarks like TheAgentCompany) — third-party measurement that disciplines internal metrics.
- **BA5** (adversarial deck critique) — process-quality measurement at the artifact stage.
- **BA1+BA3 enforcement** (citation and traceability gates) — process-quality measurement at the data stage.

Together these form the brief's "process supervision dominant; outcome supervision alongside" recipe per R14.

---

## 2. The four-stage lifecycle

```rust
// In sentinel-domain/src/ba/outcome.rs (new module)
pub struct OutcomeRecord {
    pub recommendation_id: RecommendationId,
    pub orchestration_id: OrchestrationId,
    pub requirement_refs: Vec<RequirementRef>,    // links to BA3 matrix
    pub stages: OutcomeStages,
    pub created_at: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
}

pub struct OutcomeStages {
    pub delivered: DeliveryStage,
    pub acted_on: Option<ActionStage>,
    pub metric_moved: Option<MetricMoveStage>,
    pub survived_scrutiny: Option<ScrutinyStage>,
}

pub struct DeliveryStage {
    pub delivered_at: DateTime<Utc>,
    pub delivered_to: Vec<StakeholderRef>,
    pub delivery_channel: String,
    pub artifact_ref: ArtifactReference,
    pub critique_ref: Option<ArtifactReference>,  // BA5 critique attached at delivery
}

pub struct ActionStage {
    pub action_observed_at: DateTime<Utc>,
    pub action_type: ActionType,             // Accepted | Modified | Rejected | NoExplicitDecision
    pub action_evidence: Vec<ArtifactReference>,  // links to source-of-truth confirming the action
    pub observed_via: ObservationMethod,     // Connector(name) | Interrogation(stakeholder) | OperatorMarked
    pub modifications: Option<String>,       // if action_type = Modified
}

pub struct MetricMoveStage {
    pub metric_name: String,
    pub baseline_value: f64,
    pub baseline_at: DateTime<Utc>,
    pub measurements: Vec<MetricMeasurement>,
    pub attribution_class: AttributionClass,  // CausalProven | StronglyCorrelated | WeaklyCorrelated | Uncorrelated | Confounded
}

pub struct ScrutinyStage {
    pub review_date: DateTime<Utc>,
    pub outcome: ScrutinyOutcome,            // Held | Modified | Reversed | InconclusiveReview
    pub review_evidence: Vec<ArtifactReference>,
}

pub enum AttributionClass {
    CausalProven { method: String },         // rare; requires controlled experiment or A/B
    StronglyCorrelated { confidence: f32 },  // metric moved as predicted, in the predicted window, no competing explanation
    WeaklyCorrelated { confidence: f32 },    // metric moved but competing explanations exist
    Uncorrelated { reason: String },         // metric didn't move
    Confounded { details: String },          // multiple things changed; can't isolate
}
```

Each stage's data is filled in as evidence becomes available. The record stays open indefinitely — a recommendation made in May 2026 may not have its `MetricMoveStage` populated until June or July depending on the metric's measurement cadence.

---

## 3. How data gets into each stage

### 3.1 DeliveryStage — automatic at recommendation publish

Fires from the existing publish/send/render hook chain (per BA1+BA3 enforcement). When BA-orchestrator publishes a recommendation, `OutcomeRecord` is created in the `Delivered` stage with the `DeliveryStage` fully populated.

### 3.2 ActionStage — three observation paths

**Connector observation** (preferred):
- Recommendation was "deploy feature X" → outcome connector watches the deploy pipeline; deploy detected → `ActionStage` populated with `observed_via: Connector("vercel")`.
- Recommendation was "raise prices on tier Y" → outcome connector watches Stripe/billing system; price change detected → populated.
- Recommendation was "close Linear issue Z" → outcome connector watches Linear; issue closure detected → populated.

Per `config/ba-outcomes.toml` (operator-managed), each recommendation type can specify which connector watches for what action signal.

**Interrogation observation** (fallback for actions a connector can't see):
- Schedule a follow-up interrogation (per BA2/BA4) at a configured interval (e.g., 14 days post-delivery): "did you act on recommendation X? if so, how?"
- Stakeholder's structured answer populates `ActionStage`.

**Operator-marked** (manual fallback):
- Operator opens `sentinel ba mark-action --rec <id> --action-type Accepted --evidence <url>` and the record is updated with `observed_via: OperatorMarked`.

### 3.3 MetricMoveStage — scheduled measurement

For each `OutcomeRecord` with an associated metric (declared at recommendation time via the requirement chain or operator-configured per recommendation type), sentinel schedules periodic measurements. The scheduler reads `config/ba-outcomes.toml` for cadence:

```toml
[[outcome_metric]]
recommendation_type = "pricing_change"
metric_name = "monthly_recurring_revenue"
connector = "stripe"
query = "select sum(amount) from subscriptions where status='active'"
measurement_cadence = "weekly"
attribution_window = "90d"   # how long after action to look for movement
```

The cron-style scheduler fires the measurement; the result populates `measurements: Vec<MetricMeasurement>`. The attribution class is calculated by a simple statistical procedure (out of scope here; default: if metric moved in the predicted direction within the attribution window and no major confounding signal, classify as StronglyCorrelated).

Honest about limits: causal attribution requires controlled experiments or natural experiments. Most BA recommendations don't have either. BA7 ships with classification machinery that's *honest about confidence*, not pretending to causal claims it can't make.

### 3.4 ScrutinyStage — quarterly review

Recommendations get reviewed at a configurable cadence (default quarterly). Trigger options:
- Cron job at the boundary.
- Operator explicitly opens `sentinel ba review --quarter Q3-2026 --orchestration <id>`.

The review surfaces each open `OutcomeRecord` and asks: was this recommendation held, modified, reversed, or is the review inconclusive? Operator answers; record updated.

---

## 4. Proxy metrics for hard-to-attribute recommendations

For recommendations whose direct outcome can't be measured, BA7 ships with **proxy metrics** specified per recommendation type:

| Recommendation type | Proxy metric (when direct unavailable) |
|---|---|
| "Land this PR" | PR merged + survived 30 days without revert |
| "Close this ticket" | Ticket closed + not reopened within 90 days |
| "Fix this bug" | Bug ticket closed + no regression report within 60 days |
| "Adopt this dependency" | Dependency landed + still in package.json after 90 days |
| "Run this experiment" | Experiment launched + reached pre-declared sample size |
| "Hire for this role" | Job posting created + position filled or closed within 90 days |

Each is partial. Each is much better than "the work happened." Operators extend the table for their workflows.

---

## 5. The R5 boundary — load-bearing

Outcome data is read by:
- **Operator dashboard** — "show me which recommendations from Q2 actually moved metrics."
- **A2 routing** — "which agent profiles produced StronglyCorrelated outcomes; weight their dispatch slightly higher" (this is the appraisal counter intersection).
- **Reports for stakeholders** — "here's our hit rate on financial recommendations this year."

Outcome data is **NOT** read by:
- The acting BA-orchestrator's training pipeline.
- A loop that auto-selects future recommendations based on past outcomes (the replay-mining flywheel R5 quarantines).
- Auto-promotion of prompt variants whose outcomes test "better" (the A/B-on-internal-metrics flywheel R5 quarantines).

The distinction is the load-bearing one R5 codifies. Measurement and reporting are fine. Closed-loop training on outcomes is the deception-amplifier loop. BA7's value comes from the *operator* learning, not the *agent* learning.

For the A2 appraisal-counter intersection specifically: appraisal data may consume *some* BA7 signal (a long-running pattern of WeaklyCorrelated outcomes for a model might tilt routing away from it), but the signal goes through A2's deterministic router, not back into model training.

---

## 6. Hex / DDD layering

- **`sentinel-domain/src/ba/outcome.rs`** (new module): `OutcomeRecord`, `OutcomeStages`, `DeliveryStage`, `ActionStage`, `MetricMoveStage`, `ScrutinyStage`, `RecommendationId`, `ActionType`, `ObservationMethod`, `MetricMeasurement`, `AttributionClass`, `ScrutinyOutcome`. Pure data.
- **`sentinel-domain/src/ports/outcome_attribution.rs`** (new port): `OutcomeAttributionPort` trait — `query_metric(metric_name, query, window) → Result<MetricMeasurement, _>` and `query_action_evidence(recommendation_type, since, connector) → Result<Option<ActionEvidence>, _>`. Pure trait.
- **`sentinel-domain/src/ports/outcome_store.rs`** (new port): `OutcomeStorePort` for CRUD + query on `OutcomeRecord`. Pure trait.
- **`sentinel-application/src/hooks/ba_outcome_tracker.rs`** (new): hook that fires on recommendation delivery (PostToolUse on publish/send tools per BA1+BA3 enforcement registry); creates the OutcomeRecord; registers scheduled measurements.
- **`sentinel-application/src/cron/ba_outcome_measurement.rs`** (new): scheduled job consumed by sentinel's existing cron infrastructure; runs the measurement queries; updates records.
- **`sentinel-infrastructure/src/outcome_attribution/`** (new adapter dir): connector adapters (Stripe for revenue, GitHub for PR signals, Linear for ticket signals, etc.) — most reuse the BA6 connector layer.
- **`sentinel-infrastructure/src/outcome_store/`** (new adapter dir): SQLite or JSONL adapter for the outcome store.
- **`sentinel-cli`**: new subcommands `sentinel ba mark-action`, `sentinel ba review`, `sentinel ba outcomes --quarter <q>`.
- **`config/ba-outcomes.toml`** (new, operator-managed): per-recommendation-type metric + connector + cadence configuration; proxy-metric table.

All hex/DDD-respecting. Pure value objects + pure ports in domain. Adapters in infrastructure. No new IO in `sentinel-domain`.

---

## 7. Failure modes

### 7.1 The attribution is wrong

Statistical attribution is hard. A `StronglyCorrelated` classification might be coincidence; an `Uncorrelated` might miss real impact that's confounded.

Mitigations: every attribution class names its assumptions; operator dashboard surfaces confidence prominently; aggregate trends matter more than individual records; periodic operator review is the corrective.

### 7.2 The metric moved for the wrong reason

A recommendation says "raise prices"; the operator implements it; MRR moves. But MRR also moved because a competitor went out of business that quarter. The attribution machinery has no way to know.

Mitigations: `AttributionClass::Confounded { details }` exists for exactly this case; operators are encouraged to annotate confounders manually; the dashboard surfaces them; the next-quarter scrutiny stage is the place to reclassify.

### 7.3 The metric data isn't available

Some metrics are proprietary, third-party, or only available with delay. Mitigations: BA7 marks `MetricMoveStage` as null until data arrives; the record stays open. Operators can use proxy metrics (per §4) as interim signal.

### 7.4 Recommendations without clear metrics

Some recommendations aren't directly metric-tied ("reorganize the documentation hierarchy"). Mitigations: proxy metrics (e.g., "documentation revision frequency dropped"); explicit operator-marked outcomes; or accept the record stays at `Delivered` / `ActionStage::Accepted` with no metric stage — that's a legitimate record even without a measured metric.

### 7.5 Outcome data tempts auto-promotion

Operator (or sentinel architecture) tempted to wire outcome data into agent training. R5 boundary is explicit and the sentinel-cli surfaces a warning whenever outcome data is queried in a context that *looks like* training (e.g., bulk export with model-fine-tune-shaped output). Documentation prominently flags the boundary.

### 7.6 Cross-tenant outcome leakage

Same concern as everywhere else — multi-tenant deployments must not let outcome data cross tenant boundaries. Capability tokens per consul ADR-018; Phase 1 sandbox is single-tenant.

---

## 8. Test strategy

- **Unit tests in `sentinel-domain/src/ba/outcome.rs`**: stage transitions; attribution class determination from `MetricMeasurement` history.
- **OutcomeStorePort mock**: in-memory store; CRUD + query by orchestration_id, by recommendation_id, by quarter.
- **OutcomeAttributionPort mock**: in-memory metric query returning canned values; action evidence detection.
- **Hook integration**: recommendation published → OutcomeRecord created in Delivered stage with critique attached if available.
- **Cron integration**: scheduled measurement fires; populates MetricMoveStage; AttributionClass computed.
- **CLI integration**: `sentinel ba mark-action` updates the record; `sentinel ba review` surfaces open records for a quarter; `sentinel ba outcomes` summarizes.
- **R5 boundary**: bulk outcome export contains a header warning about the R5 boundary; sentinel-cli refuses to export to known fine-tuning dataset paths.

---

## 9. Open questions

1. **Recommendation IDs** — what produces them? Recommend: BA-orchestrator generates at recommendation-emit time (UUIDv7); links to the artifact + the requirement_refs. Operator-extensible namespace if multiple orchestrators per sentinel instance.

2. **How long do outcome records stay open?** Recommend: indefinitely. The "Survived Scrutiny" stage can fire 1 year, 2 years out. Pruning is operator-driven, not automatic.

3. **Cross-orchestration recommendations** — same recommendation surfaces in multiple orchestrations. Recommend: each orchestration has its own OutcomeRecord; aggregated views can dedupe by recommendation content hash.

4. **Stakeholder-confirmed action vs metric-observed action.** A stakeholder may say "yes I did it" (interrogation) while the connector says "no the deploy didn't happen." Recommend: ActionStage carries both signals; conflicts surface to operator; truth-of-record is the connector for measurable actions, the stakeholder for non-measurable.

5. **Counterfactual estimation.** "If we hadn't shipped this recommendation, MRR would have done X instead." Out of scope for v1; flagged for future work; requires real counterfactual machinery (matched controls, A/B platform integration) that's a separate project.

---

## 10. Decision and ownership

- **Decision class:** sentinel architectural change. Adds a value-object module, three ports, two hooks (tracker + measurement cron), an adapter directory, three CLI subcommands, and one config file.
- **Owner:** Gary Somerhalder ratifies. Co-requires BA3 (traceability matrix — the substrate); co-required by A12 (external benchmarks complement internal outcomes).
- **Re-evaluation cadence:** revisit after first 100 outcome records reach the MetricMoveStage — calibrate attribution-class thresholds, refine proxy-metric defaults, evaluate operator dashboard usability.
- **Related items in the brief:** BA7 (this), BA3 (substrate), BA5 (process-quality complement), A2 (appraisal-counter consumer of outcome signal with R5 boundary), A12 (external benchmark complement), R5 (load-bearing boundary), R14 (the retirement BA7 *adds to* without violating — outcome alongside process, not outcome instead of process).

---

## 11. Methodology caveat

Attribution methodology is well-established in the causal-inference / business-analytics literature (Pearl, Imbens-Rubin, etc.). This doc applies standard practice; the novelty is *embedding measurement into the sentinel/consul/BA-orchestrator loop with the R5 boundary intact*. No new external citations needed.

## 12. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building `OutcomeRecord` + value objects, three ports, two hooks, the CLI subcommands.
- Shipping `config/ba-outcomes.toml` with documented proxy-metric defaults.
- Maintaining the R5 boundary: outcome data is operator dashboard input + dispatch input (via A2 appraisal); never training input.
- Treating BA3 as a hard prerequisite (without traceability, there's nothing to attribute to).
