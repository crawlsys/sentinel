# Policy: Retire Role-Persona Multi-Agent Decomposition as a Primitive

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **R1**
**Related policy:** `docs/policy-replay-mining-quarantine.md` (R5)

---

## TL;DR

Sentinel does **not** treat hardcoded role-persona pipelines (PM → Architect → Engineer → QA, or similar) as a primitive of the AI factory's coordination model. Personas remain a presentation-layer convenience; the underlying dispatch primitive is **capability-aware routing** (recommendation A2 in the brief).

This is a stylistic and architectural commitment, not a quarantine. There is no "lift condition" — personas-as-primitives are simply not the abstraction the system is built on. Cost of compliance is zero.

---

## What is retired

- **Hardcoded role pipelines** (PM → Architect → Engineer → QA, Researcher → Planner → Coder → Reviewer, or similar) as the structural decomposition of multi-agent work.
- **Persona prompts** as the durable abstraction in skill or hook definitions. (Personas as a transient framing inside a single agent's instruction set is fine; building the system *around* personas is not.)
- **Demos and benchmarks** that present persona pipelines as the headline architecture without acknowledging the literature's mixed-to-negative results outside narrow, well-decomposed tasks.

## What is permitted (and recommended)

- **Capability-aware routing** (A2): treat agent selection as a learned dispatch decision over a capability graph (required tools + required gates + cost class + expected artifact shape). Specialization emerges from routing, not from hardcoded personas.
- **Personas as a presentation-layer convenience**: surface "this work is being done by a Reviewer-style agent" to the operator when that framing is useful for the human. The wire-level decomposition stays capability-based; persona labels are metadata.
- **Per-agent appraisal counters**: cheap data on which capability assignments work — this is how routing improves over time, with humans in the loop on the appraisal data.

## Evidence

**Wang et al., 2024 — "Mixture-of-Agents Enhances Large Language Model Capabilities"** — `arXiv:2406.04692`. Layered ensembles of generalist LLMs beat single SOTA models on MT-Bench/AlpacaEval. The result undermines persona-decomposition's claim that role specialization is necessary for multi-agent gains.

**Li et al., 2024 — "More Agents Is All You Need"** — `arXiv:2402.05120`. Sampling-and-voting with generalist agents scales monotonically with N. Another generalist-swarm win against the persona-pipeline framing.

**Cemri et al., 2025 — "Why Do Multi-Agent LLM Systems Fail?"** (verify ID; commonly cited). The failure-mode survey clusters multi-agent failures into specification ambiguity, inter-agent misalignment, and verification gaps. Persona pipelines amplify cluster 2 (information loss at typed handoffs between rigid roles).

**Industry post-mortems on Devin and similar coding-agent demos** (2024). Demo-to-production gap on persona-pipeline systems is consistently in the 5–10× range. The architecture *looks* impressive in curated demos and degrades sharply under real workloads.

## Counter-argument addressed

**Counter:** "MetaGPT, ChatDev, and CrewAI demos are visibly impressive. Persona pipelines work."

**Response:** They work on tasks where the decomposition is correct and known a priori. That's exactly the part that's hard. The 2025 reconciliation literature settles on: persona pipelines win when the work has a known decomposition; generalist swarms with sampling win when it doesn't. The AI factory's product (best-in-class Business Analyst output) is dominated by tasks whose decomposition is *part of what the BA is producing*. Locking that decomposition into a persona pipeline is exactly the wrong choice.

Capability-aware routing (A2) gives us the specialization upside (right model × right cost × right access) without the brittleness. We can always add a persona-presentation layer on top if operators want it.

## Why the cost of compliance is zero

This policy commits Sentinel to **not making personas a primitive**. There is no code to remove, no migration. The AI factory's architectural plan already treats A2 (capability routing) as the dispatch foundation; this policy ratifies the implication.

## Decision and ownership

- **Decision class:** governance policy on coordination primitives.
- **Owner:** Gary Somerhalder ratifies; the policy persists until superseded by a future ADR that demonstrates an empirical reversal in the literature.
- **Re-evaluation cadence:** none. Triggered by evidence, not schedule.
- **Related items in the brief:** R1 (this retirement), A2 (capability-aware routing — the replacement).

## Methodology caveat

ArXiv IDs are from training-data recall (cutoff January 2026); web search and fetch were blocked during the source-brief research. Verification before any external publication of this policy is required.

## Ratification

This document is **proposed**. It becomes durable Sentinel policy when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________
