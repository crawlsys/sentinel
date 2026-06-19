# Sentinel — Work-Assurance Positioning

**Owner:** Gary Somerhalder
**Status:** Strategic positioning (origin: market-research session 2026-06-09)
**Relationship to other docs:** This doc **reframes** the category established in
[`competitive-intelligence.md`](./competitive-intelligence.md). That doc tracks sentinel as a
*"federated agent gateway with cryptographic execution proofs"* (a crypto-attestation / governance
lineage). The finding below is that sentinel's defensible category is **work-assurance / agent
correctness verification** — the proof chain is the *audit substrate*, not the value proposition.
Both framings share the same engine; this one points it at the right buyer and the right market.

---

## 1. Why this exists — the reframe

In June 2026 we ran two adversarially-verified deep market sweeps to answer "is sentinel a product,
and is anyone already building it?"

The **first sweep scoped sentinel as agent-security / EDR-for-coding-agents** and surfaced ~97
vendors — Endor Labs, Cycode, Koi (Palo Alto), Apollo Watcher, Microsoft Defender for Endpoint, etc.
That market is crowded and consolidating (SentinelOne⇠Prompt Security, Check Point⇠Lakera,
Palo Alto⇠Koi+Portkey, CrowdStrike⇠Pangea). **It is the wrong category for sentinel.** Those tools
hook coding agents to *block dangerous actions* (prompt injection, secret exfil, destructive
commands) for a **CISO / SOC buyer**. They assume the agent is potentially **malicious**.

Sentinel does something the security crowd structurally does not: it assumes the agent is potentially
**wrong, lazy, or overconfident** — and proves whether the *work was actually done correctly,
completely, and honestly*. That is the failure mode every real autonomous-coding user hits daily, and
it has a different buyer (**the engineering leader / dev-productivity team**) and a different lineage
(**CI / test-coverage / code-review-gate / eval-in-the-loop**, not EDR/firewall/DLP).

**Category: AI work-assurance — a correctness engine for autonomous coding agents.**

---

## 2. What sentinel actually is (grounded in the code)

Four pillars, each already implemented:

### Pillar 1 — Proof-of-work chains
A blockchain-style, tamper-evident chain over the agent's *work*. Each phase hashes
`(phase_id + evidence_hash + previous_hash)` and — critically — every link carries an **AI-judge
verdict** with the model that judged it.
- `crates/sentinel-domain/src/proof.rs` — `PhaseProof`, `combined_hash`, `judge_verdict`, `GENESIS_HASH`.

### Pillar 2 — Skill-as-state-machine workflow engine
Skills are ordered phases; the agent **cannot advance** to the next phase until the prior step's work
**passes an AI judge**. CI fused into the agent runtime, gating on *quality of progress* — not danger.
- `config/workflows.toml` — per-phase definitions with judge-model assignments (e.g. `linear`: claim →
  fetch → intelligence → plan-doc → worktree → review → qa-handoff → cleanup).
- `crates/sentinel-application/src/hooks/phase_gate.rs`, `step_gate.rs`, `step_judge.rs`.

### Pillar 3 — Reality-check / anti-hallucination (the sharpest wedge)
Verifies the agent's **claims against ground truth** and reopens false-done work — the thing no
competitor ships.
- `claim_reality_check.rs` + `task_completed.rs` — when a task is marked done with a concrete claim
  ("merged PR #123 / commit abc"), verify it against real **git/gh** and reopen if false.
- `step_anomaly.rs` — 9-dimensional behavioral anomaly detection ("did this step run *normally*?").
- `spec_challenge_gate.rs`, `requirements_traceability_gate.rs`, `provenance_validate.rs` —
  spec-completeness, recommendation→requirement traceability, citation provenance.
- `good_citizen_observer.rs` — nudges the agent to file tasks for issues it walked past.

### Pillar 4 — Orchestration / quality spine
`skill_router.rs` (AI routing), persistent task graphs (`task_*`), memory capture/verify
(`memory_*`), model right-sizing. The correctness backbone of an autonomous engineering org.

> Repo scale: ~90 hook modules across 7 DDD/hex crates.

---

## 3. Category & competitive white space

**Verified result:** of the 12 strongest "direct competitor" candidates put through adversarial
verification, **exactly one survived** — and it is not a product.

| Candidate | Verdict | Why not direct |
|---|---|---|
| **Praetorian "Deterministic AI Orchestration"** | ✅ direct | Architectural twin (hook-enforced gates outside the LLM, completion-promises, generator-critic dyads, self-annealing). **But it is a published whitepaper, not a sold product** (Praetorian sells Chariot, offensive security). |
| CodeRabbit / Greptile / Qodo / Graphite | ✗ | Review the *diff / human merge*, advisory to the agent. CodeRabbit docs: "Neither agent nor CodeRabbit is a mandatory gate." |
| Braintrust / LangSmith / Galileo / Confident AI / Arize | ✗ | Eval/observability; score outputs **offline/async**, gate at the CI/release boundary. Galileo's quality metric is **transcript-based** (believes the agent's claim — the opposite of false-done). |
| GitHub Spec Kit | ✗ | Scaffolding/prompts; open issues #1323 (add a verdict gate) and #1745 (add verify-it-runs) **prove the assurance layer is absent by design**. |
| GitHub false-done research | ✗ | arXiv paper (Microsoft Code|AI), pre-product, offline, doesn't gate. |
| optio / Augment Intent / Swarm Orchestrator / Good To Go | ✗ | Thin OSS or advisory; closest on individual slices, no fusion, no proof chain. |

**The fusion is the white space:** judge-gated phase advancement (in the agent runtime) +
ground-truth claim verification / false-done reopen + a tamper-evident proof-of-work chain with judge
verdicts + **vendor-neutral** over autonomous coding agents. No shipping product combines these as of
June 2026. The *pieces* are commoditizing fast; the *assembled whole* is unoccupied.

**Sources:** CodeRabbit Series B ("quality gates for AI coding") — coderabbit.ai/blog;
Greptile "independent validation layer" — greptile.com; Braintrust "evaluates after the fact, no
runtime guardrails" — braintrust.dev; Galileo Luna-2 / Action Completion (post-hoc, transcript-based)
— galileo.ai; GitHub Spec Kit issues #1323/#1745 — github.com/github/spec-kit; GitHub outcome-based
verification — github.blog ("validating agentic behavior when correct isn't deterministic");
Praetorian — praetorian.com/blog ("deterministic-ai-orchestration"); Sonar "AI Code Assurance" —
docs.sonarsource.com.

---

## 4. Buyer & demand

**ICP:** the engineering leader / platform / dev-productivity team that has rolled out autonomous
coding agents and cannot trust the output is correct, complete, and non-hallucinated.

**The pain is acute and quantified (mid-2026):**
- Lightrun 2026 State of AI-Powered Engineering: **0%** of engineering leaders are "very confident"
  AI-generated code behaves correctly in production; none could verify an AI fix in a single redeploy.
- ~**43%** of AI-generated code changes need manual debugging in production *after passing QA + staging*.
- Faros AI 2026: incidents-to-PR **+242%**, code churn **+861%**, PRs merged without review **+31%**
  under high AI adoption.
- The "AI coding agents lie about their work" / "verification is the bottleneck, not generation" /
  "AI slop" discourse is mainstream.

**Caveat — this is category-creation, not category-entry.** Buyers have the *problem* acutely but do
not yet shop for "AI work assurance" as a SKU. Analyst framing is "governance/trust-boundary," not
"work-correctness." **Naming-collision risk:** Sonar already ships "AI Code Assurance" (for the code
*artifact*, not the *agent's work*) — we must own *agent-work* assurance and differentiate clearly.

---

## 5. Architecture sketch (one page)

The product is the **EDR/OPA topology applied to work-assurance**, not "every hook calls the cloud":

```
            FUTURE CLOUD COORDINATION PLANE (Rust REST API; not in Sentinel today)
   policy authoring · report · fleet config · audit + telemetry ingest · device enrollment
                              │  delta push (NATS/JetStream or PubNub)        ▲ async audit/telemetry
                              ▼  signed policy bundle                         │ (batched, near-real-time)
   ────────────────────────── LOCAL DATA PLANE (sentinel as resident daemon) ──────────────────────────
   hooks resolve from a cached, signed policy bundle in sub-ms (named-pipe to the daemon, no per-call
   process spawn). Cloud-latency-tolerant decisions (AI judges, prod-override) route up
   synchronously; everything else is local + works offline.
```

- **Why not synchronous-cloud-per-hook:** PreToolUse fires on *every* tool call (13 hot-path gates),
  several deep. A 30–150ms round-trip per event adds seconds per turn and breaks offline use. The
  fast path must stay local.
- **Hook cloud-split (already classifiable from the code):**
  - *Local hot-path gates* (13 PreToolUse, `has_api_call=false`, git/FS/phase-state dependent) → stay local.
  - *Async-shippable observational* (~21 PostToolUse/Stop, mostly JSONL appends; tool output already
    arrives in hook stdin, so the cloud can see it) → batch + ship async.
  - *Already-cloud judge hooks* (~10 `has_api_call=true`: skill-router/Sonnet, phase-validator/Haiku,
    step-judge, memory/Qdrant) → already tolerate the cloud; these are the natural sync-decision tier.
- **Transport already exists:** the daemon (`crates/sentinel-cli/src/daemon_cmd.rs`) already runs a
  reconnecting-WebSocket uplink with a durable JSONL outbox + replay-protected inbox (the legatus
  legatus-ai-daemon client) — generalize that into the cloud sync channel rather than building new.
- **Defensibility leads with the judge + reality-check, not the crypto.** A hash chain proves
  *order/tamper-evidence*, not *correctness* — an LLM can produce a confident hallucination that
  passes a hash check. The moat is the AI-judge verdict + ground-truth claim verification; the proof
  chain is the audit substrate underneath.

---

## 6. Threats & moat

- **First-party absorption (the live threat):** GitHub, Augment, Sourcegraph, and Claude Code itself
  are building "check our own work" *inward* (self-review subagents, intent verifiers). That can
  commoditize the gate. **Moat = independence** (vendor-neutral; a third party can't be the agent
  grading its own homework) **+ the attestation artifact** an agent vendor won't bother to ship.
- **Cross-vendor is the durable wedge:** every first party governs only its own CLI and will never
  govern a competitor's. One correctness plane normalized across Claude Code + Codex CLI + Gemini CLI
  + Cursor is unserved.
- **Window is quarters, not years** — incumbents are visibly circling; speed matters.
- **Strategic fit:** this is the commercial wedge for the [Legatus platform](../README.md) thesis —
  sentinel-as-work-assurance is the quality spine of an autonomous engineering org; the fleet
  orchestration (LangGraph Rust) is the expansion product behind it.

---

## 7. Praetorian "Deterministic AI Orchestration" — the one architectural twin (and what we took)

Praetorian's [Deterministic AI Orchestration](https://www.praetorian.com/blog/deterministic-ai-orchestration-a-platform-architecture-for-autonomous-development/)
is the single candidate that survived adversarial verification as a *direct* match — but it is a
published **whitepaper, not a product** (Praetorian sells Chariot, offensive security). It independently
re-derived sentinel's core philosophy ("the LLM as a nondeterministic kernel wrapped in a deterministic
runtime"), which validates the thesis. We absorbed four of its execution mechanics — **all shipped to
`main` 2026-06-09**:

| Mechanic | Sentinel implementation | Status |
|---|---|---|
| **Completion promise** — exact terminal string, "no fuzzy interpretation" | `task_completed.rs` completion-promise claim, verified against git/gh ground truth (uncorroborated promise = hard mismatch, reopened by `claim_reality_check`) | ✅ shipped (`34b9c43`) |
| **Loop detection** (>90% output similarity) + layered redundant enforcement | `step_anomaly.rs` 10th dimension `OutputSimilarity` (line-Jaccard ≥0.90); false-done sweep now also runs on `SubagentStop` | ✅ shipped (`2b803e6`) |
| **Role-dyad serialization** (editor can't self-approve) | `workflow.rs` `RoleDyad`/`DyadVerdicts`; `advance_sequential` blocks until a *separate* reviewer/tester signs off; applied to `review` + `qa-handoff` | ✅ shipped (`ac1bcbe`) |
| **Self-annealing meta-agent** (patches the rule the agent rationalized past) | `self_annealing.rs` Stop hook: detect (always) + operator-armed (`SENTINEL_ALLOW_SELF_ANNEAL=1`) skill-patch + PR; default-deny guard never touches protected paths even when armed | ✅ shipped (`2b63460`) |

Where **sentinel stays ahead** of the twin: (1) a **cryptographic proof-of-work chain** carrying per-phase
judge verdicts (Praetorian has a mutable `MANIFEST.yaml`, no attestation); (2) **claim-vs-ground-truth
false-done detection** against real git/gh (Praetorian trusts its own tester-agent's verdict). We
deliberately did **not** adopt its state-file-trust model — that's the gameable surface our reality-check
defends against.

---

*Next review: fold into the quarterly cadence in `competitive-intelligence.md`, or split work-assurance
competitors (eval/CI/code-review lineage) from the gateway/attestation tracking there.*
