# A2 — Capability-Aware Routing

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **A2** (A-tier; foundational substrate)
**Related:**
- `docs/policy-no-role-persona-pipelines.md` (R1) — A2 is the named replacement; R1 retired the alternative; this doc specifies what we use instead
- `docs/a3-dry-run-then-commit.md` (A3) — A3's auditor seat selection is a *vendor-class* routing decision (different-vendor-from-acting); A2 generalizes that pattern
- `docs/ba5-adversarial-deck-critique.md` (BA5) — BA5's critic seat is another routing decision; same A2 substrate
- `docs/a6-reversibility-graded-tripwires.md` (A6) — routing decisions consult reversibility class (catastrophic actions route to the strongest available model regardless of cost)
- `docs/ba6-connector-layer-scoping.md` (BA6) — connector routing (which connector serves which artifact-type query) is a separate routing problem; A2's substrate generalizes but the specific connector-routing logic is BA6's
- Memory: `architecture-hexagonal-ddd`
- Memory: `model-routing-decisions` — OpenRouter / Ollama Cloud / Kimi K2.6 cost decision intersects A2 directly

---

## TL;DR

A2 introduces a **capability graph** and a **`CapabilityRouterPort`** that becomes the *substrate dispatch primitive* for every "which agent / which model for this work" decision in sentinel. Replaces the role-persona pipeline pattern that R1 retired. The core idea: don't hardcode "Reviewer" → Claude-Opus and "Coder" → Claude-Sonnet. Instead, describe each unit of work by its *required capabilities* (must produce structured JSON, must read files X-Y, must call connector Z, must operate under reversibility class C, must cost ≤ $0.05) and let the router pick the cheapest-suitable agent.

Capabilities are a *property of the work*, not a *role of the agent*. Agents declare what capabilities they *can* satisfy; work declares what capabilities it *requires*; the router matches. Per-agent appraisal counters (success rate, cost, latency) accumulate over time, feeding cheaper-suitable-agent decisions.

This is the substrate for:
- **A3's vendor-class separation** (auditor must be different vendor from acting agent → expressed as a `CapabilityRequirement::DifferentVendorFrom(acting_vendor)`)
- **BA5's critique seat selection** (same pattern, different requirement set)
- **Budget-coder routing** (per the deferred OpenRouter / Ollama Cloud / Kimi K2.6 decision — Kimi K2.6 fills the `cost: cheap, reasoning: adequate` requirement bucket)
- **Catastrophic-action routing** (per A6 — Catastrophic class routes to the strongest available model regardless of cost)

The cost of compliance is the one-time work to declare the capability graph and to add appraisal counters. The benefit is that *every* dispatch decision uses the same axis, calibration compounds, and the cost-conscious decisions (which model for which work) become legible and adjustable.

---

## 1. The architectural problem

Before A2, every "which model for this work" decision in sentinel is either:

- **Hardcoded** — A3 says "Anthropic for acting, OpenAI for auditing"; the choice lives in code.
- **Static config** — `~/.claude/settings.json` has a `model` field; one model for the whole session.
- **Implicit** — the operator's CLAUDE.md picks a model; subagents inherit; no shared policy.

The result:

- **No place to encode "cheapest model that meets these requirements"** — every cost decision is hand-coded per hook.
- **No shared appraisal data** — we can't say "Kimi K2.6 succeeded on 87% of capability-X tasks at 1/10th the cost of Opus" because there's no place that tracks it.
- **No place for cross-cutting routing rules** — "for catastrophic actions, always route to the strongest auditor" needs a coordinating substrate; today it would be repeated logic in every gate.
- **R1's retirement leaves a vacuum** — we said "don't use role personas"; the replacement (capability routing) has been named but not specified.

A2 fills the vacuum. It is *not* a model picker UI; it is a *substrate trait* every consuming hook reads when it needs to dispatch work.

---

## 2. The capability graph

### 2.1 What a capability is

A capability is a *property of work* expressed in machine-checkable form. Examples:

- `Capability::Reasoning(level: ReasoningLevel)` — required reasoning depth (Shallow / Standard / Deep)
- `Capability::ToolUse(tools: Vec<ToolKind>)` — must be able to call these tool classes
- `Capability::StructuredOutput(schema: SchemaRef)` — must produce structured output matching a schema
- `Capability::Vendor(vendor: VendorClass)` — must come from a specific vendor (Anthropic / OpenAI / Google / xAI / Ollama / OpenRouter / Other)
- `Capability::DifferentVendorFrom(vendor: VendorClass)` — must NOT be from this vendor (A3 auditor pattern)
- `Capability::OpenWeights` — must be open-weights / local-inference (for interpretability probes per A8, for the strict-privacy path per consul ADR-003)
- `Capability::LatencyBudget(ms: u32)` — must respond within budget
- `Capability::CostBudget(usd_cents_per_call: f32)` — must cost no more than budget
- `Capability::ReversibilityClass(min: ReversibilityClass)` — must be qualified to handle this class (e.g., Catastrophic-class work requires the strongest available reasoning)
- `Capability::DataLocality(zone: DataZone)` — must keep data inside the named zone (HIPAA, GDPR, on-premise)

Capabilities are *additive constraints* — a work item declares the set; the router finds the agents that satisfy all constraints.

### 2.2 Agent capability profiles

Each registered agent (model + system prompt + tool access combination) has a `AgentCapabilityProfile` declaring what it can satisfy:

```rust
pub struct AgentCapabilityProfile {
    pub agent_id: AgentId,                  // stable identity for routing
    pub display_name: String,               // for operator-facing reports
    pub vendor: VendorClass,                // Anthropic | OpenAI | Google | xAI | Ollama | OpenRouter | Other
    pub model_id: String,                   // e.g. "claude-opus-4-7", "kimi-k2-6", "gpt-5.5"
    pub provided_capabilities: Vec<Capability>,
    pub cost_per_input_token: f32,          // USD
    pub cost_per_output_token: f32,
    pub typical_latency_ms: u32,
    pub max_context_tokens: u32,
    pub data_zones: Vec<DataZone>,          // where this agent's API/inference runs
}
```

Profiles live in `config/agents.toml` (operator-managed) with sensible defaults shipped per known model. Example:

```toml
[[agent]]
agent_id = "claude-opus-4-7-strong"
vendor = "Anthropic"
model_id = "claude-opus-4-7"
provided_capabilities = [
    { Reasoning = "Deep" },
    { ToolUse = ["Edit", "Write", "Bash", "Read", "Glob", "Grep", "Task", "TaskUpdate"] },
    { StructuredOutput = "any" },
    { LatencyBudget = 30000 },
]
cost_per_input_token = 0.000015
cost_per_output_token = 0.000075

[[agent]]
agent_id = "kimi-k2-6-budget"
vendor = "Other"   # accessed via OpenRouter or direct
model_id = "kimi-k2-6"
provided_capabilities = [
    { Reasoning = "Standard" },
    { ToolUse = ["Edit", "Write", "Bash", "Read", "Glob", "Grep"] },
    { StructuredOutput = "json" },
    { LatencyBudget = 15000 },
]
cost_per_input_token = 0.000001   # ~1/15 of Opus
cost_per_output_token = 0.000005
```

The `model-routing-decisions` memory's pending OpenRouter / Ollama Cloud / Kimi K2.6 decision becomes: register the Kimi K2.6 profile in this table, set its cost and capability declarations, let the router pick it for work that fits.

### 2.3 Capability requirements per work item

A work item (a hook's dispatch decision, a skill's phase, a BA-orchestrator subtask) declares its required capabilities:

```rust
pub struct CapabilityRequirement {
    pub required: Vec<Capability>,       // must be satisfied
    pub preferred: Vec<Capability>,      // tie-breakers when multiple agents qualify
    pub forbidden: Vec<Capability>,      // disqualifiers (e.g. forbid vendor X)
}
```

For A3's auditor selection, the requirement looks like:

```rust
CapabilityRequirement {
    required: vec![
        Capability::Reasoning(ReasoningLevel::Standard),
        Capability::DifferentVendorFrom(acting_agent.vendor),
        Capability::StructuredOutput(SchemaRef::AuditorVerdict),
        Capability::LatencyBudget(5000),
    ],
    preferred: vec![
        Capability::Reasoning(ReasoningLevel::Deep),   // stronger if available
    ],
    forbidden: vec![],
}
```

For Kimi K2.6 as budget coder, the requirement is:

```rust
CapabilityRequirement {
    required: vec![
        Capability::Reasoning(ReasoningLevel::Standard),
        Capability::ToolUse(vec![ToolKind::Edit, ToolKind::Write, ToolKind::Bash]),
        Capability::CostBudget(0.05),   // 5 cents per call
        Capability::LatencyBudget(15000),
    ],
    preferred: vec![],
    forbidden: vec![],
}
```

---

## 3. The router

### 3.1 `CapabilityRouterPort` trait

```rust
// In sentinel-domain/src/ports/capability_router.rs (new)
pub trait CapabilityRouterPort {
    fn route(&self, requirement: &CapabilityRequirement) -> Result<AgentId, RoutingError>;

    fn candidates(&self, requirement: &CapabilityRequirement) -> Vec<AgentId>;

    fn explain(&self, requirement: &CapabilityRequirement) -> RoutingExplanation;
}

pub enum RoutingError {
    NoAgentSatisfies(Vec<UnsatisfiedRequirement>),
    Configuration(String),
}

pub struct RoutingExplanation {
    pub chosen: Option<AgentId>,
    pub candidates: Vec<AgentId>,
    pub eliminated: Vec<(AgentId, EliminationReason)>,
    pub tie_breakers_applied: Vec<TieBreaker>,
}
```

The router is **deterministic** for the same input: same requirement + same registered profiles + same appraisal data → same chosen agent. Tie-breakers are explicit so the operator can reason about choices.

### 3.2 Tie-breaker order

When multiple agents satisfy the `required` capabilities:

1. **Forbidden** capabilities eliminate.
2. **Preferred** capabilities accumulate score (one point per preferred-satisfied).
3. **Appraisal counters** (see §4) — higher recent success rate on similar requirements ranks higher.
4. **Cost** — cheapest passes.
5. **Latency** — fastest passes.
6. **Stable agent_id ordering** — final deterministic tie-break.

The order is configurable in `config/routing-policy.toml`; defaults shipped above.

### 3.3 Explain mode

`router.explain(req)` returns the full decision tree — what was considered, what was eliminated and why, which tie-breakers were applied. Operator-facing tooling renders this for "why did the router pick X instead of Y?" questions. Sentinel's existing `sentinel stats` CLI gains a `sentinel routing explain --requirement <id>` subcommand.

---

## 4. Per-agent appraisal counters

Every routed dispatch produces an appraisal record after the work completes:

```rust
pub struct AppraisalRecord {
    pub agent_id: AgentId,
    pub requirement_signature: RequirementSignature,   // hash of capability requirement
    pub outcome: AppraisalOutcome,                      // Success | PartialSuccess | Failure | Abandoned
    pub auditor_signal: Option<AuditorVerdict>,         // from A3 if applicable
    pub critic_signal: Option<BaCritique>,              // from BA5 if applicable
    pub actual_cost_usd: f32,
    pub actual_latency_ms: u32,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub timestamp: DateTime<Utc>,
}
```

Stored in sentinel's existing metrics store (JSONL per session, aggregated periodically). The router's tie-breaker step 3 reads these aggregates: "Kimi K2.6 has a 92% success rate on this requirement signature over the last 200 calls; Opus has 96%; cost difference is 15x; Kimi K2.6 wins on cost-adjusted success."

**Critical guardrail (intersects R5 quarantine):** appraisal data is *read by the router* but **must not be used as training input for any agent** (per R5 — replay mining quarantine). The router uses it as a *deterministic input to dispatch*, not as a *reward signal*. The distinction is load-bearing: dispatcher routing on past success is fine; training the agent to optimize the dispatcher's reward is exactly the deception-amplifier loop R5 prohibits.

---

## 5. Consuming integrations

### 5.1 A3 — auditor seat

A3's existing `select_auditor_for(acting_agent)` becomes:

```rust
let req = CapabilityRequirement {
    required: vec![
        Capability::Reasoning(ReasoningLevel::Standard),
        Capability::DifferentVendorFrom(acting_agent.vendor),
        Capability::StructuredOutput(SchemaRef::AuditorVerdict),
    ],
    preferred: vec![Capability::Reasoning(ReasoningLevel::Deep)],
    forbidden: vec![],
};
let auditor_id = router.route(&req)?;
```

A3 stops hardcoding vendor pairings. The router picks the strongest different-vendor agent the operator has configured.

### 5.2 BA5 — critic seat

Same pattern with critic-specific schema:

```rust
let req = CapabilityRequirement {
    required: vec![
        Capability::Reasoning(ReasoningLevel::Standard),
        Capability::DifferentVendorFrom(acting_agent.vendor),
        Capability::StructuredOutput(SchemaRef::BaCritique),
        Capability::ReversibilityClass(reversibility_class_of(artifact)),
    ],
    preferred: vec![],
    forbidden: vec![],
};
let critic_id = router.route(&req)?;
```

### 5.3 Budget routing for routine work

When the acting agent itself is being chosen for a routine work item (not the auditor), the requirement bias is toward cost:

```rust
let req = CapabilityRequirement {
    required: vec![
        Capability::Reasoning(ReasoningLevel::Standard),
        Capability::ToolUse(needed_tools),
        Capability::CostBudget(0.05),
    ],
    preferred: vec![Capability::LatencyBudget(5000)],
    forbidden: vec![],
};
let coder_id = router.route(&req)?;
// → Kimi K2.6 wins if the cost decision lands on it
```

### 5.4 Catastrophic-class action routing (A6 intersection)

When A6 classifies the action as Catastrophic, the requirement gains a stronger reasoning requirement:

```rust
let mut req = base_req;
if class == ReversibilityClass::Catastrophic {
    req.required.push(Capability::Reasoning(ReasoningLevel::Deep));
    req.forbidden.push(Capability::CostBudget(0.05));  // no budget shortcuts for catastrophic
}
let agent_id = router.route(&req)?;
```

The cost dimension is *forbidden* for catastrophic. The strongest agent wins regardless of price.

### 5.5 Consul peer integration

A consul peer (per consul ADR-016) — whether human-voice or automated-orchestrator — needs to route its own internal work. The router lives sentinel-side; consul peers consult it via the existing `mcp__sentinel__route_capability` MCP tool (new — add to sentinel-mcp's 11 → 12 tools). This keeps the routing substrate single-source-of-truth even across the AI-factory's external orchestrators.

---

## 6. Hex / DDD layering

- **`sentinel-domain/src/capability.rs`** (new module): `Capability`, `CapabilityRequirement`, `AgentCapabilityProfile`, `AgentId`, `VendorClass`, `ReasoningLevel`, `ToolKind`, `SchemaRef`, `DataZone`. Pure data; PartialOrd on `ReasoningLevel` for "deep at least as good as standard."
- **`sentinel-domain/src/routing.rs`** (new module): `RoutingExplanation`, `TieBreaker`, `EliminationReason`, `AppraisalRecord`, `AppraisalOutcome`. Pure data.
- **`sentinel-domain/src/ports/capability_router.rs`** (new port): `CapabilityRouterPort` trait. Pure trait.
- **`sentinel-domain/src/ports/appraisal_store.rs`** (new port): `AppraisalStorePort` trait — `record(AppraisalRecord)`, `aggregate(AgentId, RequirementSignature, window) → AggregateStats`.
- **`sentinel-infrastructure/src/routing/`** (new adapter dir):
  - `router.rs` — implementation reading profiles + appraisals; deterministic tie-break.
  - `config.rs` — parses `config/agents.toml`, `config/routing-policy.toml`.
- **`sentinel-infrastructure/src/appraisal/`** (new adapter dir): JSONL-backed appraisal store; aggregation queries.
- **`sentinel-mcp` extension**: new tool `route_capability` exposing the router to consul peers.
- **`config/agents.toml`** (new, operator-managed): agent profile registry.
- **`config/routing-policy.toml`** (new, operator-managed): tie-breaker order overrides.

All hex/DDD-respecting per `[[architecture-hexagonal-ddd]]`. Pure value objects in domain. No IO in `sentinel-domain`. All adapters in `sentinel-infrastructure`.

---

## 7. Failure modes

### 7.1 No agent satisfies requirements

`RoutingError::NoAgentSatisfies(unsatisfied)` returned. Caller decides whether to relax requirements, escalate to operator, or block. The error names every unsatisfied requirement and the agents that came closest — operator can adjust profiles or requirements.

### 7.2 Operator misconfigures a profile

Profile schema validated at config-load. Specifically: cost values non-negative, latency budgets reasonable (>0, <300_000ms), capability declarations parseable, vendor/model strings match a known registry. Validation failures surface at sentinel startup; sentinel refuses to start with invalid profiles rather than route to ghost agents.

### 7.3 Appraisal counters get gamed

The R5-quarantine boundary is critical: appraisal data is *not* a training signal, only a dispatch input. If the operator wants to disable appraisal-based tie-breaking entirely (treating routing as cost+latency only), `config/routing-policy.toml` allows skipping the appraisal step.

### 7.4 Cost values drift (vendor changes pricing)

Operator-managed. Periodic operator review of `config/agents.toml`. Could be supplemented with a `sentinel agents check-prices` command that scrapes published vendor pricing pages (out of scope for this design).

### 7.5 Vendor-class lying

If an OpenRouter-fronted call to Claude is registered as `VendorClass::Other` instead of `Anthropic`, A3's `DifferentVendorFrom(Anthropic)` check would erroneously pass. Mitigation: `VendorClass::Other` should declare its `underlying_vendor` field; `DifferentVendorFrom` checks both. Routes through OpenRouter still get correctly-attributed vendor identity.

### 7.6 Router cycle (auditor's auditor)

If A3's auditor needs its own dry-run-then-commit (which it might, for sufficiently catastrophic auditor actions), the auditor of the auditor needs a third vendor. Cycle is bounded — `DifferentVendorFrom(set)` accepts a set, not just one — but operator must register at least 3 vendor profiles for catastrophic A3-on-A3 cases. Realistic; flagged.

---

## 8. Test strategy

- **Unit tests in `sentinel-domain/src/capability.rs`**: capability equality, requirement satisfaction (single agent vs requirement), `PartialOrd` on `ReasoningLevel`.
- **Router unit tests**: each tie-breaker step in isolation; full pipeline with fixture profiles + requirements; deterministic output across multiple calls.
- **Vendor-separation tests**: A3-style requirements never pick same-vendor; OpenRouter+Anthropic via `underlying_vendor` field handled correctly.
- **Appraisal aggregation tests**: success-rate calculations; windowed aggregates (last N calls, last 24h).
- **Configuration validation tests**: malformed profiles caught at load; sentinel refuses startup.
- **Catastrophic-class routing tests**: A6 class promotion forces stronger reasoning + disables cost shortcuts.
- **Explain-mode tests**: `RoutingExplanation` is complete and traceable for every routing decision.
- **Integration with A3**: A3 hook with in-memory router; verify auditor selection picks different-vendor agent.
- **Integration with BA5**: BA5 critique hook with in-memory router; verify critic selection.

---

## 9. Open questions

1. **Capability granularity inflation.** As more capabilities get declared, the requirement space grows combinatorially. Mitigation: capability vocabulary stays small and stable; new capabilities require an ADR; per-tool / per-call capability lists are bounded.

2. **Profile staleness.** Vendor models change capability over time (a new release of Sonnet might gain a capability the old profile doesn't declare). Mitigation: profiles versioned by `model_id` exactly (`claude-sonnet-4-6-20260217`); sentinel doesn't infer cross-version compatibility.

3. **Geographic / data-zone routing as v1 or v2?** `DataZone` capability is sketched here but real implementation needs operator UX for declaring zones, vendor metadata for where calls actually run, and policy enforcement. Recommend v2; v1 ships without `DataZone` enforcement, just the field reserved.

4. **Appraisal write amplification.** Every dispatch produces a record; high-volume sessions generate lots of writes. Mitigation: batched async write to the appraisal store; in-memory accumulation flushed periodically.

5. **Cross-tenant routing.** When multiple AI-factory orchestrators share a sentinel instance, do they share the appraisal store? Recommend: per-tenant aggregation; tenant boundary inherits from the same capability-token machinery as BA6/BA5/A3 (consul ADR-018 territory).

---

## 10. Decision and ownership

- **Decision class:** sentinel architectural change. Adds two new ports, two adapter directories, two config files, and one new MCP tool.
- **Owner:** Gary Somerhalder ratifies. Replaces R1 (already-retired role-persona pipelines). Consumed by A3, BA5, and any future routing decision. Intersects directly with the deferred OpenRouter / Ollama Cloud / Kimi K2.6 cost-comparison decision (the agent profile registry is where that decision becomes operational).
- **Re-evaluation cadence:** revisit after 10K routing decisions accumulated — calibrate tie-breaker weights, prune unused capabilities, review appraisal aggregation windows.
- **Related items in the brief:** A2 (this), R1 (the retired alternative), A3 (consumer), BA5 (consumer), A6 (catastrophic-routing intersection), BA6 (related connector-routing pattern), R5 (quarantine boundary: appraisal data is dispatch input, not training signal).

---

## 11. Methodology caveat

The bitter-lesson framing (per R1's evidence base — Wang `2406.04692` MoA, Li `2402.05120` More Agents) backs this approach: routing on capability is more robust than persona-based decomposition. RouteLLM (`arXiv:2406.18665`) is the most directly analogous published result — learned routing over capability classes beats hand-designed pipelines. ArXiv IDs from training-data recall (cutoff January 2026); verification required before external publication.

## 12. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building `CapabilityRouterPort` + adapter, `AppraisalStorePort` + adapter, the two config files.
- Shipping default agent profiles for known frontier models.
- Refactoring A3 and BA5 to consume the router (replaces their hardcoded vendor selection).
- Adding `route_capability` to sentinel-mcp's tool surface.
- Treating R5 as the quarantine boundary: appraisal data is dispatch input, never training signal.
- Resolving the OpenRouter / Ollama Cloud / Kimi K2.6 cost decision as the first concrete profile registration.
