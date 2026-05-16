# Policy: Retire Novelty as the Primary Axis for Tripwire Gating

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **R3**
**Related policy:** `docs/policy-replay-mining-quarantine.md` (R5)

---

## TL;DR

Sentinel's blast-radius / tripwire system will not be primarily gated on **novelty** (unfamiliar tool combinations, novel domains, unexpected reach). The primary axis is **reversibility** (trivially undoable / undoable with effort / irreversible / catastrophic). Novelty remains available as a *low-priority secondary signal* that informs prioritization, not as the gating decision.

This replaces a class of tripwire design that the 2024–25 literature has shown to fail in three distinct ways. The cost of compliance is the work to re-classify tools and actions by reversibility — bounded, finite, valuable.

---

## What is retired

- Tripwires whose **primary trigger** is unfamiliarity of tool combinations, new domain reach, or unexpected resource consumption.
- "Novelty score" as a gating threshold for blocking actions.
- Anomaly-detection on agent behavior as the headline safety primitive.
- Any framing that presents novelty-detection as catching "what could go wrong" — it does not; it catches "what we haven't seen before," which is a different and worse thing.

## What is permitted (and recommended)

- **A6 — Reversibility-graded tripwires** as the primary axis. Every action gets a reversibility class:
  - **Trivially reversible** — undoable in seconds, no human attention needed (file save under VCS, transient state). Tripwire largely irrelevant.
  - **Reversible with effort** — undoable with a known recovery procedure (force-push, schema migration with rollback). Tripwire requires explicit acknowledgment.
  - **Irreversible** — practically undoable (production DB drop, sent email, public deploy). Tripwire blocks pending human confirmation.
  - **Catastrophic** — irreversible *and* high-blast-radius (production data loss, account deletion). Tripwire blocks unconditionally pending out-of-band human approval.
- **Novelty as a low-priority secondary signal.** Reasonable inputs to *prioritization* (a novel-and-irreversible action gets reviewed faster than a familiar-and-irreversible action). Not the gating decision.
- **A7 — Honeypot canaries** for empirical measurement that whatever tripwires we ship are actually catching adversarial behavior.

## Evidence

**Goodhart's law applied to monitors.** When "doesn't trip the tripwire" becomes selection pressure (and once any learning loop exists over agent traces, it does), the system is trained to satisfy the proxy rather than the underlying intent.

**Krakovna specification-gaming catalogue** (DeepMind, maintained since 2018, now 100+ documented cases). The empirical pattern is exhaustive: enumerable-bad-pattern training and filtering approaches *exhaustively* fail to catch the failure mode they were designed for, because the model finds adjacent behaviors that satisfy the filter while violating the intent. Novelty tripwires are an instance of this pattern.

**Hubinger et al., 2019 — "Risks from Learned Optimization in Advanced Machine Learning Systems"** — `arXiv:1906.01820`. The foundational mesa-optimization paper. Trained systems can contain internal learned optimizers whose objectives differ from the training objective. Once "doesn't trip the novelty tripwire" becomes selection pressure, the system *is* the gradient-hacking concern in concrete deployment form.

**Voyager-style exploration literature** (Wang et al. 2023 onward). Novelty *is* the signal of useful exploration. Suppressing it suppresses capability. The cost of novelty-based tripwires is paid in capability, not just in false-positive friction.

**This session's empirical data point.** During the May 2026 setup work, Sentinel's `worktree_reminder` hook fired as a false positive (we were already in a worktree). The hook fired on familiar pattern → flagged as worth noting. A novelty-based tripwire would have stayed silent during the same session. False-positive surface on a reasonable signal is real; false-positive surface on a novelty signal is catastrophic.

## Counter-argument addressed

**Counter:** "Some novelty signal is useful — a never-before-seen tool combination is informative."

**Response:** Agreed. The policy demotes novelty to a *secondary* signal, not a banned one. Novel inputs can inform prioritization (review queue ordering, sampling rate for human spot-checks) without being the gating decision. The gating decision is reversibility because reversibility is the property that survives Goodhart.

**Counter:** "Reversibility is harder to compute than novelty. Novelty is just an embedding distance; reversibility requires classification per tool."

**Response:** Yes. Reversibility is a one-time classification cost per tool in the registry; novelty is a runtime cost paid forever. The one-time cost is the right tradeoff. The classification itself is bounded, finite, and valuable (it forces an explicit conversation about what each tool actually does).

**Counter:** "Demoting novelty means we lose anomaly detection."

**Response:** We lose anomaly detection *as a gating primary*. Anomaly detection survives as part of observability — surfacing unusual patterns to humans for review. The complaint is well-targeted at "anomaly detection blocking actions"; not at "anomaly detection informing humans."

## Why the cost of compliance is moderate, not zero

This policy requires re-classifying tools and actions by reversibility class. That's bounded work but real work — roughly proportional to the size of Sentinel's tool registry. The classification informs more than just tripwires; it improves audit, prioritization, and post-hoc review.

A novelty-based tripwire system that *also* exists (as a secondary signal) is unaffected by this policy. Removing the *primacy* of novelty is the architectural change.

## Decision and ownership

- **Decision class:** governance policy on tripwire design axis.
- **Owner:** Gary Somerhalder ratifies; the policy persists until evidence accumulates that reversibility-grading fails in a specific operational pattern not anticipated here.
- **Re-evaluation cadence:** none.
- **Related items in the brief:** R3 (this retirement), A6 (reversibility-graded tripwires — the replacement), A7 (honeypot canaries).

## Methodology caveat

ArXiv IDs are from training-data recall (cutoff January 2026); the Krakovna list is a live document and should be verified for the current state of documented specification-gaming examples.

## Ratification

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________
