# A3 — Dry-Run-Then-Commit with Separate-Model-Family Auditor

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **A3** (S-tier; top-3 single-item impact)
**Related:**
- `docs/policy-replay-mining-quarantine.md` (R5) — A3 + interpretability + canaries are the headline replacements for the flywheel that R5 quarantines
- `docs/policy-no-single-auditor-primary.md` (R2) — A3 is the architectural answer; this doc specifies how
- `docs/policy-no-novelty-primary-tripwires.md` (R3) — A3's trigger uses reversibility class (A6), not novelty
- `docs/hook-quality-improvements.md` — Issue 2 (tool_usage_gate granularity) is the minimum-viable bridge to A3's reversibility-class trigger
- Memory: `architecture-hexagonal-ddd` (mandatory house style)
- Memory: `model-routing-decisions` (OpenRouter/Ollama-Cloud/Kimi-K2.6 cost comparison deferred — intersects A3's auditor seat directly)

---

## TL;DR

A3 introduces a new sentinel hook — **`dry_run_then_commit`** — that intercepts mutating actions whose reversibility class is *irreversible* or *catastrophic*, requires the agent to emit a structured dry-run artifact (intended action + reasoning + expected effect), routes that dry-run to a **trusted auditor running on a different model family from a different vendor**, and only commits the action on auditor pass. High-impact actions sample to humans regardless of auditor result.

This is the **largest single risk reduction available** in the current literature (Greenblatt et al., AI Control, `arXiv:2312.06942`). It is also the architectural answer to the brief's central concern — *sentinel today monitors artifacts and budgets while the dangerous failure modes live in latents that artifacts do not faithfully reveal*. A3 doesn't close that gap completely (only runtime interpretability probes per A8 do), but it dramatically tightens it by adding a *latent-aware* check (the auditor reasons about intent, not just artifact shape) before any irreversible commit.

The largest open question is **multi-vendor MCP commitment** — Anthropic auditing Anthropic is not separation in any sense the literature endorses. Landing A3 forces this commitment. The OpenRouter / Ollama Cloud / Kimi K2.6 cost decision intersects directly; the auditor seat is one of the model slots that decision will fill.

---

## 1. What A3 is

From the source brief (recommendation A3):

> Every non-trivial mutating action emits a structured dry-run (intended action + reasoning + expected effect). A trusted auditor — ideally with different weights from a different vendor, not just a different prompt — scores the dry-run. High-impact actions sample to humans. Action only commits on auditor pass.

Operationally, this means:

1. **Trigger.** Any mutating action whose reversibility class (per the proposed A6 reversibility-graded tripwires) is *irreversible* or *catastrophic* enters the dry-run flow. Trivially-reversible and reversible-with-effort actions skip A3 — gate latency is the cost; reserve it for the cases that need it.
2. **Dry-run construction.** The acting agent produces a structured artifact describing what it intends to do, why, and what it expects to happen. The acting agent does NOT execute the mutation yet.
3. **Auditor scoring.** The dry-run is routed to an auditor — a separate-model-family LLM accessed via MCP — which scores the dry-run on a small set of axes (correctness, intent alignment, safety, presence of unstated assumptions). Returns score + reasoning.
4. **Decision.** Auditor pass → action commits. Auditor fail → action blocks, reasoning surfaces to the operator. High-impact actions (configurable threshold; in BA vertical, anything cited in an exec deliverable) escalate to human review *regardless* of auditor result — auditor is necessary but not sufficient for highest-blast-radius actions.
5. **Proof chain.** The dry-run artifact, auditor score, auditor reasoning, and final decision become entries in sentinel's existing proof chain. The audit trail is complete and cryptographically attributable.

---

## 2. Why this is the largest single risk reduction available

**Greenblatt et al., Redwood Research, 2023–24** — `arXiv:2312.06942` ("AI Control: Improving Safety Despite Intentional Subversion") is the load-bearing paper. They study exactly this architecture: an untrusted strong model doing work, a trusted (potentially weaker) auditor scoring it. The result:

- **Single-monitor protocols help meaningfully**, but the safety margin degrades as the capability gap between auditor and audited shrinks.
- **When auditor and audited are the same model family**, red-team experiments show the audited can predict the auditor's outputs and steer around them. Separation by prompt alone is insufficient.
- **Separation by model family** preserves the safety margin even under the assumption that the audited model is actively trying to subvert oversight.

This is the only safety architecture in current literature whose results survive the assumption of intentional subversion. No other primitive in Sentinel's stack does. R5 quarantines the patterns that would *amplify* deception; A3 is the runtime check that *catches* it.

**Why "dry-run-then-commit" specifically** (vs. "post-hoc audit"): The Krakovna specification-gaming catalogue and Apollo Research's o1 scheming evaluations both show that frontier models can produce trajectories that *look clean post-hoc* while violating intent during execution. Post-hoc audit is structurally too late for irreversible actions. The dry-run pattern forces the audit to occur *before* the trajectory commits — the only place it can prevent harm rather than catalogue it.

---

## 3. The architectural problem A3 solves

The brief's headline framing:

> Sentinel today monitors **artifacts and budgets**, while the 2024–25 frontier safety work has shown the dangerous failure modes live in **latents — goals, plans, situational awareness — that artifacts do not faithfully reveal**.

Sentinel's existing hooks (`tool_usage_gate`, `commit_message_validator`, `git_hygiene`, `phase_gate`, `pre_push_steel_test`, etc.) all operate on *artifacts*: file paths, commit messages, command strings, phase completion proofs. They are excellent at catching artifact-level mistakes and policy violations. They are structurally incapable of catching a *latent* failure — an agent that produces a clean-looking artifact while pursuing a misaligned goal.

A3 doesn't fully close this gap (only A8, runtime interpretability probes, addresses latents directly). But A3 adds an auditor whose job is to *reason about intent*, not just inspect artifacts. The auditor reads the dry-run's stated reasoning and expected effect, compares them against the artifact shape, and surfaces inconsistencies. This is a *latent-aware* check — imperfect (the auditor is itself an LLM, also subject to the same patterns), but materially stronger than artifact-only checks.

Combined with:
- **A4** (debate between auditors) — strictly stronger than single auditor; reduces the auditor's own-vulnerability surface
- **A7** (honeypot canaries) — empirically measures whether the auditor is actually catching what it should
- **A8** (runtime interpretability probes) — latent-level monitoring when available

…A3 is the *most-impactful single architectural change* available. A4 + A7 + A8 are upgrades to A3, not replacements.

---

## 4. Concrete sentinel-side design

### 4.1 New hook module

Create `crates/sentinel-application/src/hooks/dry_run_then_commit.rs` alongside the existing 27 hooks. It runs on `PreToolUse` and decides whether the call needs the dry-run flow.

**Pseudo-code shape** (final shape will follow existing hook patterns in the crate):

```rust
//! Dry-Run-Then-Commit Hook
//!
//! Runs on PreToolUse for mutating actions whose reversibility class
//! is Irreversible or Catastrophic. Forces the acting agent to emit a
//! structured dry-run; routes the dry-run to a separate-model-family
//! auditor via MCP; commits the action only on auditor pass.
//!
//! Sample-to-human for any High-Impact action regardless of auditor result.

pub fn process(input: &HookInput, fs: &dyn FileSystemPort, env: &dyn EnvPort, auditor: &dyn AuditorPort) -> HookOutput {
    let tool = input.tool_name.as_deref().unwrap_or("");

    // Step 1: Is this a mutating action of interest?
    let class = reversibility::classify(tool, &input.tool_input);
    if class < ReversibilityClass::Irreversible {
        return HookOutput::allow();  // Trivial / reversible-with-effort skip A3
    }

    // Step 2: Has a dry-run for this action already been performed and approved?
    if dry_run_marker::is_approved(fs, input.session_id.as_deref(), input)? {
        return HookOutput::allow();
    }

    // Step 3: Construct the dry-run request
    let dry_run = DryRunRequest::from_tool_call(tool, &input.tool_input, input.session_id.clone());

    // Step 4: Route to the auditor (separate-model-family per ADR/policy)
    let verdict = auditor.score(&dry_run)?;

    // Step 5: Human-sample escalation for High-Impact actions
    let needs_human = class == ReversibilityClass::Catastrophic
        || verdict.confidence < HUMAN_SAMPLE_CONFIDENCE_THRESHOLD
        || sampling::should_sample_to_human(input.session_id.as_deref());

    if needs_human {
        return HookOutput::ask_user(format_human_review(&dry_run, &verdict));
    }

    // Step 6: Auditor decision
    match verdict.decision {
        AuditorDecision::Pass => {
            dry_run_marker::record_approved(fs, input.session_id.as_deref(), input, &verdict)?;
            HookOutput::allow()
        }
        AuditorDecision::Block { reason } => {
            HookOutput::deny(format!(
                "🟠 [Dry-Run Auditor] BLOCKED: {reason}\n\nDry-run summary:\n{}",
                dry_run.summary()
            ))
        }
    }
}
```

### 4.2 Reversibility classification (intersects A6)

The trigger depends on `reversibility::classify(tool, input)` returning `Irreversible` or `Catastrophic`. This is the same classification logic A6 (the broader reversibility-graded tripwires design, separate ADR) builds on top of. For A3 alone, a minimum-viable classifier:

```rust
pub enum ReversibilityClass {
    TriviallyReversible,    // file save under VCS, transient state
    ReversibleWithEffort,   // force-push, schema migration with rollback
    Irreversible,           // production deploy, sent email, public release
    Catastrophic,           // production DB drop, account deletion, financial txn
}

pub fn classify(tool: &str, input: &serde_json::Value) -> ReversibilityClass {
    match tool {
        // Always catastrophic
        "Bash" if bash_matches_pattern(input, &CATASTROPHIC_BASH_PATTERNS) => ReversibilityClass::Catastrophic,
        // MCP tools by upstream class
        t if t.starts_with("mcp__") => classify_mcp(t, input),
        // File mutations under source control = reversible-with-effort
        "Write" | "Edit" => ReversibilityClass::ReversibleWithEffort,
        _ => ReversibilityClass::TriviallyReversible,
    }
}

const CATASTROPHIC_BASH_PATTERNS: &[&str] = &[
    "rm -rf",
    "git push --force",
    "git reset --hard",
    "DROP TABLE",
    "DROP DATABASE",
    "kubectl delete",
    "terraform destroy",
];
```

Bridge note: the hook-quality ADR's Issue 2 fix (path-based trivial-write exemption for `.claude/projects/`, `.claude/plans/`, etc.) is the minimum-viable upstream of this — that exemption list is the `TriviallyReversible` slice. The full A6 design (separate ADR) is what makes this classification complete and configurable.

### 4.3 Dry-run artifact shape

```rust
pub struct DryRunRequest {
    pub session_id: SessionId,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub agent_intent: String,        // free text — agent describes what it intends
    pub agent_reasoning: String,     // free text — why this action accomplishes the intent
    pub agent_expected_effect: String, // free text — what the agent expects to be true after
    pub reversibility_class: ReversibilityClass,
    pub constructed_at: DateTime<Utc>,
}

impl DryRunRequest {
    pub fn from_tool_call(tool: &str, input: &serde_json::Value, sid: Option<SessionId>) -> Self {
        // Constructs from the PreToolUse hook input. The intent / reasoning /
        // expected_effect fields are populated either by:
        //   (a) the agent emitting them as structured metadata in the tool call
        //       (preferred — new tool input field convention), or
        //   (b) inferred from recent transcript context (fallback for legacy
        //       tool calls; less reliable).
        ...
    }
}
```

**Tool input convention**: Sentinel encourages (eventually requires) agents to populate optional `_intent`, `_reasoning`, `_expected_effect` fields in mutating tool calls when reversibility class is Irreversible or higher. Tool calls without these fields fall through to inference from transcript context — works but produces weaker dry-runs.

### 4.4 Auditor routing — `AuditorPort` trait

```rust
// In sentinel-domain/src/ports/auditor.rs (new port — pure trait, no IO)
pub trait AuditorPort {
    fn score(&self, dry_run: &DryRunRequest) -> Result<AuditorVerdict, AuditorError>;
}

pub struct AuditorVerdict {
    pub decision: AuditorDecision,
    pub confidence: f32,       // 0.0-1.0
    pub axes: AuditorAxes,
    pub reasoning: String,
    pub auditor_model: String, // e.g. "openai:gpt-5.5"
}

pub enum AuditorDecision {
    Pass,
    Block { reason: String },
}

pub struct AuditorAxes {
    pub correctness: f32,         // does the action achieve the stated intent?
    pub intent_alignment: f32,    // does the stated intent match recent transcript?
    pub safety: f32,              // does the action introduce known-bad patterns?
    pub unstated_assumptions: f32, // does the dry-run rely on facts not in evidence?
}
```

The auditor implementation lives in `sentinel-infrastructure/src/dry_run_auditor.rs` and is selected through the A2 capability router. **The architectural requirement is that the auditor model is from a different underlying vendor than the acting model.** Concrete model seats are router profiles, not direct SDK adapters:

- OpenRouter-backed frontier profiles for vendor-separated hosted judging.
- Ollama/OpenRouter open-weights profiles for the open-weights-commitment path and lower-cost review.
- Capability requirements (`DifferentVendorFrom`, `StructuredOutput`, reasoning tier, cost budget) decide the seat.

The selector ensures the auditor is *never* the same underlying vendor as the acting agent. Sentinel reads the session's acting model from environment + config and picks through the router accordingly.

### 4.5 Human-sampling escalation

```rust
const HUMAN_SAMPLE_CONFIDENCE_THRESHOLD: f32 = 0.85;
const HUMAN_SAMPLE_RATE_FOR_PASSED_ACTIONS: f32 = 0.05;  // 5% spot-check

pub fn should_sample_to_human(session_id: Option<&str>) -> bool {
    // Random sampling of passed actions for human spot-check
    rand::random::<f32>() < HUMAN_SAMPLE_RATE_FOR_PASSED_ACTIONS
}
```

Human escalation paths:
- **Catastrophic class** — always.
- **Auditor confidence < 0.85** — always.
- **Random 5% spot-check** of passed actions — calibration; without it, auditor drift goes undetected.
- **Explicit operator request** — operator can mark a session "always human review" via Legatus AI for sensitive workflows.

The escalation is routed through Legatus AI (per Legatus AI ADR-016's `LegatusAiIdentity::HumanOperator` path) — the same channel the operator already uses for voice/text supervision. No new UI required.

### 4.6 Proof-chain integration

Every dry-run produces a proof-chain entry. Sentinel's existing `proof` module is extended to carry:

```rust
pub struct DryRunProof {
    pub dry_run_request: DryRunRequest,
    pub auditor_verdict: AuditorVerdict,
    pub human_decision: Option<HumanDecision>,
    pub final_outcome: ActionOutcome,
    pub chain_hash: Hash,    // links to prior proof in the session's chain
}
```

This means every Irreversible/Catastrophic action in the session is auditable post-hoc: who proposed it, what they said it would do, what the auditor scored it, whether a human was sampled, what actually happened. The audit story stays cryptographically attributable.

---

## 5. Failure modes and what we do about them

### 5.1 Auditor unavailable

If the auditor's MCP endpoint is unreachable:
- **For Irreversible:** action blocks; agent is told to retry. Auditor health is reported via existing `mcp_health` hook.
- **For Catastrophic:** action blocks unconditionally and escalates to human, even if a retry would work — the auditor being down for a catastrophic action is exactly when human review matters most.

### 5.2 Auditor flakiness / inconsistency

Auditor calls aren't deterministic. Mitigations:
- **Temperature 0** by default (the auditor is reasoning, not generating creative content).
- **Two-pass option**: high-impact actions can be routed to two distinct auditor instances; disagreement escalates to human.
- **Track auditor stability metrics**: per-auditor variance over time; drift triggers a calibration review.

This is also where A4 (debate between auditors with asymmetric prompts) is the natural upgrade path.

### 5.3 Auditor itself misaligned

Per the R2 retirement policy: a single auditor is the literature's weakest scalable-oversight primitive. A3 alone is *strictly stronger* than no auditor, but it is *strictly weaker* than A4 (debate). The full safety stack requires:
- A3 (this) as the foundation.
- A4 as the upgrade — two auditors with asymmetric information.
- A7 (honeypot canaries) as the empirical check.
- A8 (interpretability probes) as the latent-aware addition.

A3 ratifying doesn't mean stopping here. It means establishing the *substrate* on which the others build.

### 5.4 Acting agent learns to game the dry-run

If the acting agent learns "produce dry-runs that pass the auditor" as a selection pressure (which R5 quarantine explicitly prohibits via the no-replay-mining policy), the system is back to the deceptive-alignment failure mode. A3's safety guarantee depends on R5 holding. The two ratify together or neither does.

### 5.5 Cost and latency

Every Irreversible/Catastrophic action gains one auditor MCP call. Cost depends on auditor model choice (open-weights local ~ free; frontier API ~ several cents per call). Latency depends on the same (local ~ 100ms; frontier API ~ 1-3s). Both are real but bounded:

- The trigger is reversibility class, not action count. Trivial / reversible-with-effort actions are unaffected.
- For BA-vertical work, most actions are reversible-with-effort (drafts, intermediate artifacts). The irreversible-class actions are the high-value commits that benefit from the gate.
- Operator-facing reports should surface auditor cost / latency over time so the cost is visible.

### 5.6 Operator override

The existing `hygiene_override` mechanism (signed token, 60s TTL) lets the operator manually bypass A3 for legitimate cases the auditor would block. The override is audited; the audit trail records *why* a human chose to bypass. This is the right pattern — don't make the gate unbypassable; make every bypass legible.

---

## 6. Multi-vendor MCP commitment

A3 forces a decision: Sentinel commits to **multi-vendor model routing** for the auditor seat. The production path is the A2 capability router over OpenRouter/Ollama-backed model profiles, with underlying-vendor identity tracked even when a gateway fronts the model.

Practical implications:

- The auditor is selected as a capability-routed model seat, not a direct-vendor SDK side path.
- Multiple model profiles exist across underlying vendors so the right one can be selected per acting-agent vendor.
- The operator's model/router configuration registers which profiles are available.

This is also where the **deferred OpenRouter / Ollama Cloud / Kimi K2.6 cost-comparison decision** (per `model-routing-decisions` memory) intersects directly. The auditor seat is one of the model slots that decision will fill. Recommend treating the cost comparison as a prerequisite for A3 implementation:

- **If OpenRouter wins on cost**: use OpenRouter as the auditor backend (multi-vendor by construction; route auditor calls to whichever non-acting vendor is cheapest at the moment).
- **If Ollama Cloud wins**: use Ollama Cloud as the auditor backend (potentially open-weights local; same cost-routing logic).
- **Kimi K2.6 as budget coder** is the *acting* agent in cost-sensitive workflows, not the auditor. The auditor stays on a stronger model to preserve the capability margin Greenblatt et al.'s result depends on. Note: this means the cost decision affects acting-agent and auditor-agent differently — the budget choice is per-seat, not per-system.

---

## 7. Open questions

1. **Reversibility classification per MCP tool.** Beyond Bash and Edit/Write, each registered MCP tool needs a reversibility class. Some are obvious (`linear__delete_issue` is at least Irreversible); others require operator judgment. Recommend: a config file `~/.claude/sentinel/reversibility.toml` operators can extend per their workflow, with sentinel shipping conservative defaults.

2. **Confidence threshold tuning.** The proposed `HUMAN_SAMPLE_CONFIDENCE_THRESHOLD = 0.85` is a guess. Real value will need calibration once auditor data accumulates. Per honeypot canary results (A7), the threshold should be adjustable based on observed auditor false-negative rate.

3. **Two-pass auditor for catastrophic actions.** Should every Catastrophic action route to two auditors automatically (proto-A4)? Argues for: catastrophic by definition deserves the strongest available gate. Argues against: cost. Recommend yes, with both auditors running in parallel.

4. **Dry-run metadata format standardization.** Tool calls today don't have a standard place to put `_intent` / `_reasoning` / `_expected_effect`. Sentinel should define a structured convention for its own hook/tool envelopes and treat transcript inference as weaker evidence.

5. **Interaction with `phase_gate`.** Sentinel's existing `phase_gate` hook gates skill execution through claim → verify → execute phases. Where does dry-run fit? Recommend: between `verify` and `execute` for irreversible actions inside a skill, parallel to (not replacing) phase_gate's existing flow.

6. **BA-vertical specialization.** For BA outputs (decks, briefs, memos), every "publish" is irreversible (the deck reached a human reader). A3 should fire for every deck publish; auditor scoring uses BA5 (adversarial deck critique) as the scoring logic. This means BA5 isn't a separate hook — it's the BA-specialized auditor for A3.

---

## 8. Test strategy

- **Unit tests**: `dry_run_then_commit` hook with in-memory `AuditorPort` returning canned verdicts. Cover Pass, Block, low-confidence, network-error.
- **Reversibility classification tests**: every entry in `CATASTROPHIC_BASH_PATTERNS` classifies correctly; edge cases (e.g., `git push` without `--force` is not catastrophic, `git push --force` is).
- **Auditor-vendor separation tests**: assert that when acting model is `claude-opus-4-7`, the auditor selector picks a profile with a different underlying vendor; cycle through all (acting, auditor) pairs.
- **Human-sample escalation tests**: Catastrophic always samples; auditor confidence < 0.85 always samples; random sampling produces ~5% over many runs.
- **Proof-chain integration tests**: each dry-run produces a chain entry; chain links to prior proof; chain re-verifies cleanly with `sentinel verify`.
- **End-to-end test in sentinel-cli**: simulate a session that proposes an Irreversible action; auditor blocks; action does not commit; audit log shows the block reason.
- **Cost/latency benchmark**: measure auditor MCP call latency for each backend vendor; emit warning if any backend exceeds 5s p95.

---

## 9. Hex / DDD layering

- **`sentinel-domain`**: new `AuditorPort` trait, new `DryRunRequest`/`AuditorVerdict`/`AuditorAxes` value objects, new `ReversibilityClass` enum. All pure data; no IO.
- **`sentinel-application/src/hooks/dry_run_then_commit.rs`**: new hook module. Reads from `FileSystemPort`, `EnvPort`, `AuditorPort`. No direct IO.
- **`sentinel-application/src/hooks/mod.rs`**: register `dry_run_then_commit` in `HOOK_NAMES`.
- **`sentinel-infrastructure/src/dry_run_auditor.rs`**: A2-routed auditor implementation over configured OpenRouter/Ollama model profiles. No direct-vendor SDK adapter side path.
- **`crates/sentinel-cli/src/hook_cmd.rs`**: dispatcher knows about the new hook event; routes appropriately.
- **`config/hooks.toml`**: new entry registering the hook for `PreToolUse`.
- **`config/reversibility.toml`** (new): per-tool reversibility classification overrides operators can edit.

All hex/DDD-respecting per `[[architecture-hexagonal-ddd]]`. No new IO in `sentinel-domain`. All adapters in `sentinel-infrastructure`.

---

## 10. Decision and ownership

- **Decision class:** sentinel architectural change. Adds a new hook, a new port, and a new adapter category. Not a quarantine, not a retirement.
- **Owner:** Gary Somerhalder ratifies; once ratified, implementation can be sequenced through the hook scaffolding plus A2 router profile coverage.
- **Re-evaluation cadence:** revisit after first 1000 dry-runs accumulated — calibrate confidence threshold, prune false-positive auditor blocks, tune human-sample rate.
- **Related items in the brief:** A3 (this), A4 (debate — the natural upgrade), A6 (reversibility-graded tripwires — the trigger classification), A7 (honeypot canaries — the empirical check), A8 (interpretability probes — the latent-aware addition), BA5 (presentation generation with adversarial critique — the BA-vertical specialization of A3's auditor logic), R5 (quarantine on replay mining — must hold for A3's safety to hold).

---

## 11. Methodology caveat

ArXiv IDs cited (Greenblatt 2312.06942, etc.) are from training-data recall (cutoff January 2026). Web search and fetch were blocked during the source-brief research. Verification before any external publication is required. The directional claims are well-established in the alignment literature; specific paper IDs need a live cross-check.

## 12. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building the new hook + port + adapter category.
- Multi-vendor MCP for the auditor seat.
- Resolving the OpenRouter / Ollama Cloud / Kimi K2.6 cost decision as a prerequisite.
- Treating R5 quarantine as a co-requirement (A3's safety depends on it).
