# Sentinel → AI Factory: Exec Summary

**For:** Gary | **From:** Jared | **Date:** 2026-05-14 | **Full brief:** `sentinel-ai-factory-brief.md`

## The product

**Best-in-class Business Analyst at factory scale.** Surfaces documentation, generates presentations, defends every recommendation against cited business requirements — using data gathered through automated discovery (system pulls) and interrogated discovery (structured stakeholder Q&A). Sentinel is the substrate; the BA vertical is the product.

## The gap (one sentence)

Sentinel today is excellent scaffolding against an agent trying to help, and almost no defense against an agent trying to look like it is helping. The plan below closes that gap as honestly as the 2024–25 safety literature allows.

## S-tier — product-critical (6 items)

| # | Recommendation | Why |
|---|---|---|
| 1 | **R5 — Quarantine open-ended replay mining** | Zero build cost, largest single risk removed. Mining proof chains to fine-tune behavior is a deception amplifier (Greenblatt et al. on alignment faking, `arXiv:2412.14093`). |
| 2 | **BA6 — Documentation-surface connector layer** (Confluence, Notion, Jira, Linear, Drive, SharePoint, Slack, Gmail) | Without data, no BA product. Every other primitive is downstream. |
| 3 | **BA1 — Citation-locked decision artifacts** (every claim source-pinned) | Defines the output quality bar that distinguishes BA work from generic LLM output. |
| 4 | **BA3 — Requirements traceability matrix** (need → requirement → recommendation → outcome) | Makes recommendations-without-stated-need structurally impossible. Audit trail is sellable. |
| 5 | **BA5 — Adversarial critique on every deck/brief/memo** | Auto-confident decks are the highest-blast-radius BA output. Non-negotiable quality gate. |
| 6 | **A3 — Dry-run-then-commit with separate-model-family auditor** | Only credible runtime-safety architecture in the literature (Greenblatt et al., `arXiv:2312.06942`). |

## A-tier — high impact (8 items)

A2 capability-aware routing • BA2 two-mode discovery • BA4 stakeholder interrogation protocol • A13 spec-challenge • A6+R3 reversibility-graded tripwires (replacing novelty) • A1 Magentic-One dual-ledger in Legatus AI • A12 TheAgentCompany + BA-eval baseline • BA7 outcome attribution

## B-tier — medium impact (15 items)

Mostly cleanups and upgrades that follow naturally once S/A items land. Includes A4 debate-between-auditors • A5 PRM gating • A7 honeypot canaries • A11 graph memory • A14 flow analytics • R1/R2/R4/R7/R8/R9/R10/R13/R14 retirements • R12 capability-token migration. See full brief for details.

## C-tier — low impact, research-grade, or contingent (8 items)

A8 interpretability probes (contingent on open-weights commitment) • A9 MCP/A2A standards adoption • A10 bounded ADAS over hook graph • A15 (subsumed by BA7) • R6/R11/R15 doc-only retirements.

## The single most important sentence

**Process supervision on artifacts (what Sentinel does today) does not in principle detect deception (what the latest literature shows is the dangerous failure mode).** The fix is dry-run-then-commit + separate-model-family auditor (A3), reversibility-graded gating (A6), honeypot canaries (A7), and a hard quarantine on the self-improvement flywheel (R5). Skip any one of those and the safety story is not defensible.

## What we keep

Phase gates. Cryptographic proof chains. `[Sentinel-Authority]` provenance prefix. Hook engine as policy plane. Worktree-isolated agent execution. TaskList as cross-session governance. These are the moat and the foundation.

## What we drop / quarantine

**Quarantine:** open-ended replay mining + auto-authored hooks (R5). **Retire as primitives:** role-persona pipelines (R1), single-auditor safety story (R2), novelty-primary tripwires (R3), free-form agent chat (R9), shared blackboards (R10), pure vector RAG as team memory (R13). **Reframe:** treasuries as cost-not-safety (R4), denylists as secondary-not-primary (R7+R12), auto-summary without critique (R8).

## Strategic positioning

- **Most agent vendors:** substrate looking for a vertical (LangGraph, AutoGen, CrewAI, Bedrock AgentCore, Vertex Agent Builder).
- **Us:** substrate built around a product spec (best-in-class BA), generalizable later.
- **Unique primitives nobody else has:** cryptographically attributable process supervision, requirements traceability as a governance object, interrogation-as-protocol, hook engine over cross-vendor MCP, reversibility-graded gating.

## Decisions needed from Gary

1. **Quarantine R5?** Reads to some as "giving up the flywheel." Reads to me as "refusing to build a deception amplifier." Your call.
2. **Multi-vendor MCP commitment** so A3 has a separate-model-family auditor (Anthropic auditing Anthropic is not separation)?
3. **Open-weights commitment** so A8 (runtime interpretability) becomes possible? (Without this, A8 stays C-tier.)
4. **BA-eval corpus** — do we have access to historical BA deliverables we can score against, or do we curate from public sources?

## What this brief is and isn't

**Is:** an opinionated, defensible map of where the field has moved in the last 18 months, mapped onto Sentinel's primitives, with one-line counterarguments addressed per item.

**Isn't:** a verified citation list. Web search was blocked during research; ArXiv IDs are training-data recall (cutoff Jan 2026) and need a verification pass before any external use. Directional claims are solid; specific IDs are not yet underwritten.

---
**Full brief with per-item defense, counterarguments, and citations:** `sentinel-ai-factory-brief.md` (~9,000 words)
