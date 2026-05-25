# Agentic Software Factory — Architecture & Gap Analysis

> Source: deep-research report "Building an Agentic Software Factory with Claude
> Code, Opus, Sonnet, Codex, Kimi, Rust, Axum, and Sentinel" (2026-05-25),
> reconciled against sentinel's *actual* implementation and against live
> OpenRouter probes run the same day.

## TL;DR

The report's recommended architecture is a **governed multi-model factory**:
Claude Code as the operator cockpit, a Rust/Axum control plane (Sentinel) in
front of every meaningful tool action, specialist work delegated to Codex/Kimi
workers, and every decision producing a proof-chained receipt.

**Sentinel already implements most of this.** The report reads less like a
spec to build from scratch and more like a description of what's already here.
This doc maps the report's blueprint onto sentinel's real code and isolates the
genuine gaps.

## Model strategy — reconciled with live evidence

The report's capability mapping (Opus = hard reasoning, Sonnet = daily
execution, Codex = autonomous/CI loops, Kimi = long-context worker) matches
Gary's original intent and survived a **live OpenRouter probe (2026-05-25)**.
Two empirical corrections the report also predicted:

1. **Verdict quality is not the differentiator.** Every callable model judged a
   weak-evidence adversarial prompt correctly (`sufficient: false`, conf
   ≥ 0.95). The real differentiators are **callability on the key** and
   **latency**.

2. **Non-Claude models need explicit pinning, not catalog discovery.** The
   report: *"gateway discovery only adds `claude*`/`anthropic*` IDs; use
   explicit model pinning."* This is the same root cause as the live
   `moonshotai/kimi-k2.6` 404 — provider routing, not model quality. Sentinel's
   `JudgeModel` enum pins exact slugs, which is precisely the prescribed
   pattern.

### Sentinel `JudgeModel` tiers (post-fix, live-verified)

| Tier | Slug | Live latency | Role |
|------|------|-------------|------|
| `Haiku` | `openai/gpt-5.4-nano` | 1.3s | fast/routine gate |
| `Kimi` (default) | `moonshotai/kimi-k2-thinking` | 10s | long-context review, Eastern-distribution diversity |
| `Sonnet` | `openai/gpt-5.5` | 2.8s | cross-vendor verification (report's "GPT-5.5 for complex Codex coding") |
| `Opus` | `anthropic/claude-opus-4.7` | 10s | critical-strict final review |

Critical-strict trio = moonshot + openai + anthropic → genuine cross-vendor
disagreement signal, all confirmed callable.

## Gap analysis: report blueprint vs. sentinel reality

| Report component | Sentinel status | Gap |
|------------------|-----------------|-----|
| **Local signed command shim** (fail-secure, not HTTP) | ✅ Hooks run as `sentinel hook --event` command invocations; deny via `HookOutput::deny`. Fail-secure by construction. | Report wants Ed25519-signed envelopes on the shim→broker hop. Sentinel runs in-process (no broker hop), so the threat differs — but the `[Sentinel-Authority]` prefix is the provenance equivalent. |
| **Append-only proof chain (SHA-256 + Ed25519)** | ✅ `sentinel-domain/proof.rs` + `step_proof.rs`: SHA-256 combined-hash chain, optional Ed25519 `signature`. | Ed25519 signing is *optional* (`signature: None` common path). Report wants it mandatory for high-assurance. M1.7 ceiling per [[hsm_pq_scoped_out]]. |
| **Model router (MR)** | ✅ `capability.rs` + `capability_router.rs` (A2 capability-aware routing). | Report's MR also fronts Kimi/Codex *workers*; sentinel's router selects *judges/auditors*. Worker-delegation surface is thinner. |
| **Worker delegation (Anthropic/Kimi/Codex)** | ⚠️ `anthropic.rs`, `rig_judge.rs`, `dry_run_auditor.rs` via OpenRouter (single gateway fronts all). No dedicated Codex-CLI / Kimi-thinking-mode workers. | Report wants model-native worker adapters + a task queue (`POST /v1/tasks`). Sentinel has no task-delegation API. **Genuine gap.** |
| **Prometheus `/metrics`** | ✅ `/legatus/metrics` (hand-rolled exposition, added in Tier 3 hardening). | Report wants the full metric set (hook latency, policy denies, approvals pending, task queue depth). Partial. |
| **Approvals API + glass-break overrides** | ✅ `hygiene_override` + phase-gate override TTL (3600s). | Report wants scoped, signed, dual-control override *tokens* as first-class objects with proofchain entries. Sentinel's override is coarser. |
| **`Stop` gate enforces completion** | ✅ `verification_gate`, `task_completed`, `teammate_idle` hooks. | Aligned. |
| **Reversibility-graded tool gating** | ✅ A6 `LayeredReversibilityClassifier` (4-layer). NOTE: just fixed the PowerShell/sequential-thinking deadlock (commit f9b3120). | Report doesn't cover this — sentinel is *ahead* here. |

## The genuine gaps (where the report adds value)

1. **Worker-delegation task API.** Sentinel governs the *acting* agent but has no
   `POST /v1/tasks` to delegate sub-work to a Codex CI worker or a Kimi
   long-context scan. This is the report's biggest net-new idea. Pattern:
   factory tasks `delegate_codex`, `delegate_kimi_context_scan`,
   `delegate_security_review` behind approved wrappers — *not* the Claude Code
   `/model` picker.

2. **Mandatory Ed25519 on the proof chain.** Today optional. For audit-grade /
   EU-AI-Act provenance, the `AuditGrade` trust tier should require it.

3. **First-class glass-break override tokens.** Scope + issuer + approver +
   ticket + TTL + max-use + dedicated signing key, each use → proofchain entry.
   Replaces the current coarser `hygiene_override`.

4. **Codex as autonomous CI worker.** The report is firm: Codex's hook surface
   is incomplete (command handlers only, partial `PreToolUse`), so Codex belongs
   as a *delegated worker / CI engine*, never the governed entrypoint. Sentinel
   has no Codex worker adapter yet.

## Architectural posture the report validates

- **Claude Code stays the governed shell** (richest hook/permission surface) —
  sentinel already assumes this.
- **HTTP hooks are fail-open; command shims are fail-secure** — sentinel is
  command-shim native. ✅
- **Don't push non-Claude models into the `/model` picker** — pin them. The
  `JudgeModel` enum does exactly this. ✅

## Proposed next steps (decompose into work items)

1. **`delegate_*` worker task API** (Axum `POST /v1/tasks` + `GET /v1/tasks/{id}`)
   with Codex-CLI and Kimi-thinking-mode adapters. *Largest gap, highest value.*
2. **Mandatory Ed25519 for `AuditGrade` tier** — flip `signature` from optional
   to required when `records_provider()` is true.
3. **Glass-break override tokens** — replace coarse `hygiene_override` with
   scoped, signed, TTL-bound tokens.
4. **Finish the voiceprint/CatastrophicAck protocol surface** (the half-built
   `consul-protocol` feature blocking the engine rebuild — see Deferred below).

## Deferred / blocked (discovered this session)

- **Engine rebuild blocked** by a pre-existing half-built feature:
  `sentinel-legatus` references `consul_protocol::messages::AckDecision`,
  `RegisterSession.operator_id`, `ConsularMessage::CatastrophicAck`, and
  `ChallengeNonce::to_hex` — none of which exist yet in `consul-protocol`.
  (The `consul-domain::identity::republic` half was fixed this session, commit
  `ebf7284` on consul-agent, which unblocked sentinel-domain/application/
  infrastructure.) Until the protocol surface is finished, the new judge slugs
  are merged to source but not live in the running engine — the previously
  staged engine (with the PowerShell fix) remains active.
