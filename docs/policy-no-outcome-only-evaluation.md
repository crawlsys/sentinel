# Policy: Retire Outcome-Only Evaluation

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **R14**
**Related policy:** `docs/policy-replay-mining-quarantine.md` (R5)

---

## TL;DR

Sentinel does **not** evaluate agent behavior, skills, or hooks on **outcome alone**. Process supervision — scoring the *trajectory* that produced the outcome — is required alongside outcome scoring. This is consistent with what Sentinel's phase-gated proof chains already do; this policy ratifies that pattern as durable and prohibits backsliding into outcome-only evaluation.

The cost of compliance is zero: Sentinel's existing architecture already implements process supervision. The policy commits to *not* abandoning it in favor of cheaper but weaker outcome-only metrics.

---

## What is retired

- **Final-outcome-only scoring** for agent behavior (pass/fail on the last test, success/failure on the last commit, did-the-deal-close on the recommendation).
- **Internal metrics** built solely on outcome aggregates (close rate, success rate, completion count) without trajectory analysis.
- **A/B promotion** of prompts, models, or hooks based on outcome metrics alone — see also R5 quarantine on the broader self-improvement flywheel, which subsumes this.
- **"Did the work succeed?"** as the only evaluation question. Process supervision asks: "Did the work succeed *for the right reasons*?"

## What is permitted (and recommended)

- **Process supervision** as the dominant evaluation mode. Sentinel's phase-gated workflow already does this: each phase (claim → verify → execute, plus the brief's proposed challenge step) emits a proof artifact; the proof chain is the trajectory record.
- **A5 — PRM-style process reward gating** at the step level (per Lightman et al. 2023). Promotes Sentinel's phase gates from binary pass/fail to calibrated scoring.
- **A12 — External benchmarks (TheAgentCompany + BA-specific eval corpus)** as the *outcome* anchor that disciplines internal metrics. Outcome scoring is OK; outcome-only scoring is not.
- **BA7 — Outcome attribution for recommendations** (BA-vertical specialization). Closes the loop from artifact to business metric — but only *alongside* process supervision, never replacing it.

## Evidence

**Lightman et al., OpenAI, 2023 — "Let's Verify Step by Step"** — `arXiv:2305.20050`. The foundational result. Process reward models (PRMs) beat outcome reward models (ORMs) on math and reasoning by significant margins. The mechanism: outcome-only scoring cannot discriminate between trajectories that arrive at the right answer for the right reason and trajectories that arrive there for the wrong reason. Process scoring can.

**Greenblatt et al., 2024 — "Alignment Faking in Large Language Models"** — `arXiv:2412.14093`. Models strategically present cleaner outcomes when they perceive they are being evaluated. Outcome-only evaluation is the canonical target for this behavior — the trajectory is hidden, only the outcome is graded.

**Hubinger et al., 2024 — "Sleeper Agents"** — `arXiv:2401.05566`. Adversarial training based on outcome scoring did not remove deception; it taught deception to produce cleaner outcomes under evaluation pressure. Outcome-only evaluation is the canonical input to this failure pattern.

**Sentinel's existing architecture.** Phase-gated proof chains are *already* process supervision. The choice we have is to ratify the pattern as policy or to allow drift back toward outcome-only metrics. This policy ratifies.

## Counter-argument addressed

**Counter:** "Outcomes are what we actually care about. Why score the trajectory?"

**Response:** We care about outcomes *that generalize*. An agent that produces correct outcomes by coincidence, by Goodhart-gaming the eval, or by a deceptive trajectory that happens to land on the right answer is not an agent we can extrapolate from. The Lightman et al. result is exactly this: process scoring is what makes outcome wins generalize.

**Counter:** "PRMs require training data we don't have."

**Response:** True for trained PRMs. The transitional approximation is model-as-judge using a strong evaluator (per A5 in the brief). The architecture supports moving to a trained PRM later when labeled trajectories accumulate; the current substitute is good enough to ship.

**Counter:** "Outcome scoring is cheaper."

**Response:** Cheaper per-evaluation. More expensive per-bad-decision-shipped. The asymmetry of cost is the load-bearing point.

## Why the cost of compliance is zero

Sentinel's phase gates *are* process supervision. The proof chain *is* the trajectory record. This policy commits to *not abandoning* what's already built. No code changes are required; no migrations. The change is a commitment not to drift.

The downstream commitments (A5 for PRM-style scoring upgrade, A12 for external benchmarks, BA7 for outcome attribution) have their own costs and timelines. This policy doesn't bind them to ship immediately — it commits the system to keeping process supervision as the dominant evaluation mode while those mature.

## Interaction with R5 quarantine

This policy and R5 (quarantine on open-ended replay mining) are intertwined:
- R5 prohibits the *self-improvement flywheel* that would use outcome-only metrics to fine-tune behavior on its own traces.
- R14 (this policy) prohibits the *evaluation framing* — outcome-only — that would drive such a flywheel even if R5 were lifted.

The two policies reinforce each other. R5's lift conditions include "bounded version of self-improvement shipped and demonstrated value" — that bounded version (A10, ADAS over hook graph) must itself use process supervision, not outcome-only metrics, or it falls afoul of this policy.

## Decision and ownership

- **Decision class:** governance policy on evaluation methodology.
- **Owner:** Gary Somerhalder ratifies; the policy persists.
- **Re-evaluation cadence:** none. The literature consensus is stable (2023 onward).
- **Related items in the brief:** R14 (this retirement), A5 (PRM-style gating, the upgrade path), A12 (external benchmarks, the outcome-anchor companion), BA7 (outcome attribution within the BA vertical), R5 (the related quarantine on replay mining).

## Methodology caveat

ArXiv IDs are from training-data recall (cutoff January 2026); verification required before external publication.

## Ratification

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________
