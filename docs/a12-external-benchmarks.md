# A12 — External Benchmarks (TheAgentCompany + BA-Eval Corpus)

**Status:** Proposed methodology (pending Gary's ratification of direction)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **A12** (A-tier; external anchor)
**Related:**
- `docs/policy-replay-mining-quarantine.md` (R5) — load-bearing boundary: benchmark scores are *measurement*, not training signal
- `docs/policy-no-outcome-only-evaluation.md` (R14) — benchmarks are outcome-shaped measurement that *complements* process supervision
- `docs/ba7-outcome-attribution.md` (BA7) — internal outcome attribution; A12 is the external complement
- `docs/ba5-adversarial-deck-critique.md` (BA5) — BA-eval corpus uses BA5-shaped scoring rubric
- `docs/a2-capability-aware-routing.md` (A2) — benchmark results may feed agent-profile appraisal counters (with R5 boundary)
- Memory: `model-routing-decisions` — cost decisions (OpenRouter/Ollama-Cloud/Kimi-K2.6) should be informed by per-model benchmark performance, not vendor marketing

---

## TL;DR

Sentinel runs **two external benchmarks** on a recurring cadence:

1. **TheAgentCompany** (Xu et al., 2024 — `arXiv:2412.14161`) — the generic anchor. Simulated company with Gitea + RocketChat + OwnCloud + NPC coworkers. Frontier baseline today is ~24-30% full-task completion. Run as the *general* AI-factory capability measurement.

2. **BA-Eval Corpus** — a sentinel-curated benchmark for the BA-vertical product specifically. Real BA deliverables (advisory briefs, exec memos, board decks) sourced from public corpora and (with permission) partner orgs. Scored against a rubric that mirrors BA5's critique axes plus outcome-realism axes. Run as the *BA-vertical-specific* measurement.

Together these are the **external discipline** that prevents internal metrics from drifting into Goodhart territory. The brief is explicit: internal metrics will optimize themselves into local maxima; external benchmarks we don't control are the corrective.

Critical R5 boundary: benchmark *scores* feed reports, methodology decisions (which substrate changes ship), and (with the deterministic-dispatch-input boundary) A2 appraisal counters. Benchmark scores do **not** feed agent training. The benchmark exists to evaluate the substrate, not to train against.

This doc specifies methodology and cadence, not architecture. There is no new hook, no new port. There IS a new CLI subcommand (`sentinel eval`) and a new config file (`config/eval-corpora.toml`).

---

## 1. Why external benchmarks are A-tier

The brief's framing:

> Internal metrics drift toward Goodhart. TheAgentCompany and similar external anchors are the only credible discipline. Adopt this *before* any of the metrics infrastructure ships.

Without external benchmarks:
- Internal metrics (cycle time, proof-chain cleanness, auditor pass rate) measure *the system optimizing itself*.
- Any improvement on internal metrics is ambiguous — did the architecture actually improve, or did the agents learn to look better against the metrics?
- The cost decisions (per `model-routing-decisions` memory: OpenRouter vs Ollama Cloud vs Kimi K2.6) rest on vendor marketing rather than honest comparison.
- We can't credibly tell external stakeholders "we improved" — we can only say "our internal scores moved."

With external benchmarks:
- Architectural changes ship with a before/after measurement on a benchmark we don't control.
- Cost decisions rest on per-model benchmark performance rather than per-vendor pricing alone.
- External claims (papers, talks, sales) are anchored in third-party-runnable measurements.
- The "are we actually getting better?" question has a deterministic answer.

---

## 2. Benchmark 1: TheAgentCompany

### 2.1 What it is

Per Xu et al., 2024 — `arXiv:2412.14161` — TheAgentCompany is a simulated software company environment. The agent acts as an employee with access to Gitea (source), RocketChat (comms), OwnCloud (docs), and a set of NPC coworkers (LLM-driven characters playing PM, eng, support roles). Tasks include real-world things like: triage a bug, write a feature spec, respond to a stakeholder request, schedule a meeting, debug a CI failure.

Tasks are scored on full-task completion. As of the paper's publication, frontier models (Claude 3.5 Sonnet, GPT-4o) completed roughly 24-30% of full tasks end-to-end. Newer frontier models likely score higher; this needs measurement (see methodology caveat).

### 2.2 Why it's the right generic anchor

- **Real-world shape.** Coordination across systems of record, stakeholder interaction, long-horizon work. Not toy.
- **Hard to game.** The tasks are diverse and the scoring is end-to-end; optimizing for one task doesn't help on others.
- **Frontier ceiling exists.** ~24-30% means there's substantial headroom; an architectural improvement that moves us from 28% → 40% is a real, defensible claim.
- **Public.** Anyone can re-run and verify our reported scores.

### 2.3 Cadence

- **Baseline run** (run NOW, before any of the brief's architectural changes ship): establish the pre-substrate-change baseline. Lose this and we lose the counterfactual.
- **Re-run on major substrate change**: every time one of the S-tier or A-tier items lands (A3, A6, A2, etc.), re-run on the affected task subset.
- **Quarterly full re-run**: comprehensive measurement on the full task set quarterly. Tracks long-term trends.
- **Vendor change re-run**: any time we change the model behind a major agent seat (acting, auditor, critic), re-run on a sample subset.

### 2.4 Procedure

```
sentinel eval theagentcompany --baseline               # first-time baseline
sentinel eval theagentcompany --subset coordination    # subset re-run
sentinel eval theagentcompany --full                   # quarterly comprehensive
sentinel eval theagentcompany --compare <baseline-id>  # diff against prior run
```

Results stored at `~/.claude/sentinel/eval/theagentcompany/`:
- `runs/{run_id}.jsonl` — per-task results
- `summary/{run_id}.json` — aggregated scores
- `baselines/` — symlinks to baseline runs

Each run records: timestamp, sentinel version, agent profiles used (per A2), substrate version (which of the brief's items are active), full per-task transcript.

---

## 3. Benchmark 2: BA-Eval Corpus (sentinel-curated)

### 3.1 Why a separate BA-vertical benchmark

TheAgentCompany measures generic coordination. It does not measure:
- Citation discipline (BA1).
- Requirements traceability (BA3).
- Critique quality (BA5).
- Discovery completeness (BA2/BA4).
- Outcome attribution honesty (BA7).
- Tonal calibration on exec-facing content.

The brief calls this out: TheAgentCompany alone is "necessary but insufficient" for the BA vertical. The fix is a *BA-specific* eval corpus.

### 3.2 Corpus composition

A BA-eval corpus is a set of `EvalCase` entries:

```rust
pub struct EvalCase {
    pub case_id: String,                   // stable identifier
    pub stakeholder_brief: String,         // the original request (verbatim, possibly redacted)
    pub source_corpus: SourceCorpus,       // where the materials live
    pub gold_artifact: Option<GoldArtifact>,  // the human-authored "good" output, if available
    pub gold_outcomes: Option<GoldOutcomes>,  // what actually happened in the real world
    pub scoring_rubric: ScoringRubric,
    pub provenance: CaseProvenance,        // who contributed; license; consent
}

pub enum SourceCorpus {
    Public { url: String, license: String },  // e.g., publicly-published advisory deliverables
    PartnerContributed { partner: String, redaction_level: RedactionLevel },
    SyntheticGenerated { generation_method: String },  // for filling gaps in real cases
}
```

Cases are sourced from three pools:

1. **Public corpora** — published advisory deliverables (McKinsey/Bain/BCG public reports, academic case studies, public think-tank briefs). Scrape + curate + score-rubric attach.
2. **Partner-contributed** — with explicit permission, partner orgs share past BA work (redacted as needed). High signal because these are *real* deliverables with *real* outcomes; rare because of confidentiality.
3. **Synthetic generated** — for filling gaps in capability coverage (e.g., we have lots of strategy briefs but few pricing analyses). Synthetic cases are explicitly typed as such; not used for outcome-scoring (the synthetic outcome isn't real).

### 3.3 Scoring rubric

Per case, the agent's output is scored on:

- **Citation density + accuracy** (mirrors BA5 axis 3.3 + 3.1) — every claim cited; citations match source; right type of source.
- **Requirements coverage** (mirrors BA5 axis 3.5 + BA3) — every gold-recommendation is addressed or explicitly trade-off-ed; no recommendations untraceable to stated need.
- **Alternatives seriousness** (mirrors BA5 axis 3.2) — top-2 alternatives steelmanned; not strawmanned.
- **Tonal calibration** (mirrors BA5 axis 3.4) — confidence proportional to evidence; no spin; explicit uncertainty where warranted.
- **Outcome realism** (NEW — only available where `gold_outcomes` is present) — agent's recommendation matches or substantively reasons about what actually happened. Rare to score (most cases lack outcome data); when available, very high signal.
- **Stakeholder fit** — output is shaped appropriately for the stated audience (exec / board / customer / internal team).

Each axis 0.0-1.0; weighted score per case; aggregate per run.

### 3.4 The R5 boundary, applied

The corpus and its scores are READ by:
- Report (which substrate changes improved which axes).
- A2 appraisal counters (with R5 deterministic-dispatch-input boundary).
- Methodology decisions (does this architectural change ship or not).
- External reporting (when we tell stakeholders our BA-eval scores).

The corpus and its scores are NOT READ by:
- Agent training pipelines.
- Auto-promotion of prompt variants based on score deltas.
- Any closed-loop training-against-the-benchmark machinery.

Specifically: **the corpus has a private test split that the agents are never exposed to during any prompt iteration**. Public training split is OK for prompt iteration *with operator oversight*; private test split is held back for honest measurement. Cross-contamination of public into private invalidates the test.

### 3.5 Corpus curation cadence

- **Initial corpus**: 50 cases sourced from public corpora before any of the brief's architectural items ship. Cover 5 BA archetypes (strategy, pricing, ops, M&A, marketing) × 10 cases each.
- **Quarterly expansion**: add 10-20 cases per quarter; rotate scoring rubric review.
- **Partner outreach**: pursue partner-contributed cases asynchronously; high signal but slow.

Corpus storage at `~/.claude/sentinel/eval/ba-corpus/`:
- `cases/{case_id}.json` — case definition + materials
- `runs/{run_id}.jsonl` — per-case scoring
- `private-test-split/` — held back; never exposed to agents during prompt iteration

---

## 4. CLI surface

```
sentinel eval list                                      # list available corpora + run history
sentinel eval theagentcompany [--baseline|--subset|--full|--compare <id>]
sentinel eval ba-corpus [--public|--full] [--compare <id>]
sentinel eval ba-corpus --add-case <case-spec.json>     # curate
sentinel eval report <run_id>                           # human-readable summary
sentinel eval diff <run_id_a> <run_id_b>                # before/after comparison
```

All results write to `~/.claude/sentinel/eval/`. Sentinel's existing local API exposes the results at `/api/eval/...` endpoints.

---

## 5. Operating discipline

### 5.1 The pre-change baseline is sacred

**No S-tier or A-tier architectural change ships without a baseline run of BOTH benchmarks first.** Without the pre-change baseline, there is no counterfactual; the change cannot be honestly assessed.

This is the strongest single rule in this doc. It applies to A3, A6, A2, BA1+BA3 enforcement, BA5, BA6, BA7 — every architectural change in the brief.

### 5.2 The post-change re-run is required

Each architectural change is followed by a re-run on the affected subset. The result is recorded in the change's audit trail. If the change made things worse on an external benchmark, the operator has explicit data and can choose to revert.

### 5.3 Vendor swap re-run

The cost decision (OpenRouter / Ollama Cloud / Kimi K2.6 per the `model-routing-decisions` memory) ships *with* a benchmark comparison: each candidate vendor runs the same subset; results are public to the decision-making process. No vendor selection on cost alone.

### 5.4 No goal-Goodharting

If sentinel team-internal pressure pushes scores up over time on the same fixed corpus, that's a red flag. Mitigations: corpus rotation (private test split sees rotation; public-side prompt iteration is separate); operator audit of score-vs-real-outcome correlation periodically; external publication of methodology (third parties can replicate).

---

## 6. Hex / DDD layering

This doc adds *very little* architectural surface. There is no new domain port, no new hook. The implementation lives at the application/CLI layer:

- **`sentinel-application/src/eval/`** (new module): per-corpus runners; `TheAgentCompanyRunner`, `BaCorpusRunner`; scoring logic. Pure logic; consumes existing ports.
- **`sentinel-cli`**: new `sentinel eval` subcommand tree.
- **`sentinel-infrastructure/src/eval/`** (new adapter dir): corpus storage (filesystem-backed); run history storage.
- **`config/eval-corpora.toml`** (new): per-corpus configuration (paths, rubric weights, R5-boundary enforcement settings).

The "no new domain port" is intentional. Evaluation is application-layer concern. The benchmark runners *use* the agents (via A2's `CapabilityRouterPort`) but don't need their own port — they're consumers, not providers.

---

## 7. Failure modes

### 7.1 Benchmark contamination

If the benchmark cases leak into agent training, the score becomes meaningless. Mitigation: private test split; no agent ever sees private-split cases until a measurement run; cases that show evidence of contamination (suspiciously perfect scores on novel-shaped cases) are quarantined and replaced.

### 7.2 Corpus drift from real BA work

Sentinel-curated corpus is curated by sentinel team; over time the corpus may drift away from what real BAs actually do. Mitigation: partner-contributed cases anchor corpus to real-world distribution; quarterly review of corpus shape vs. operator-reported real workflow shape.

### 7.3 Scoring rubric over-fits

The scoring rubric reflects BA5's critique axes; if the BA5 critic itself drifts, the rubric drifts. Mitigation: rubric versioning; rubric changes require explicit operator decision + full corpus re-score; old-rubric scores preserved for trend analysis.

### 7.4 The R5 boundary slips

Someone wires benchmark scores into agent training because it's "obviously" useful. Mitigation: documentation prominently flags R5; sentinel-cli `eval export` refuses to write to paths matching fine-tune dataset patterns; periodic operator review.

### 7.5 Cost of running benchmarks

A full TheAgentCompany run is expensive (lots of model calls across many tasks). Mitigation: run quarterly comprehensive + per-change subset; subset runs are 5-10% the cost; comprehensive runs are infrequent. Budgeted in the sentinel operator's cost model.

### 7.6 BA-corpus partner consent

Partner-contributed cases require explicit, ongoing consent. Mitigation: per-case `provenance.consent_expiry` date; consent renewal workflow; cases auto-archive when consent lapses.

---

## 8. Open questions

1. **Which model evaluates BA-corpus outputs?** Recommend: A2-router picks a strong model from a *different vendor* than the model being measured (same separate-family pattern as A3's auditor). Avoids self-grading.

2. **Public vs private test split sizing?** Recommend: 70/30 public/private; rotate 10% of private into public per quarter as new cases arrive.

3. **External publication.** Do we publish methodology + scores? Recommend yes (it's a credibility thing); publish methodology fully + summary scores; private-split contents stay private but score *distributions* publish.

4. **BA-corpus as a community asset.** Could a shared BA-eval corpus emerge across teams? Out of scope; but the data structures (`EvalCase`, `ScoringRubric`) are designed to be portable if/when that emerges.

5. **TheAgentCompany version drift.** The benchmark itself evolves; results across versions aren't directly comparable. Mitigation: record benchmark version in every run; cross-version comparison goes through explicit reconciliation.

---

## 9. Decision and ownership

- **Decision class:** methodology + tooling. Adds CLI surface + corpus storage + run history; no new domain abstractions.
- **Owner:** Gary Somerhalder ratifies direction; operator runs benchmarks on the documented cadence.
- **Re-evaluation cadence:** annual review of methodology (rubric, corpus shape, R5 boundary enforcement); per-architectural-change application of the pre/post discipline (§5).
- **Related items in the brief:** A12 (this), R5 (boundary), R14 (outcome alongside process), BA7 (internal outcome complement), BA5 (rubric source), A2 (consumer of appraisal-counter aggregates), `model-routing-decisions` memory (cost decision should run through this benchmark).

---

## 10. Methodology caveat

The TheAgentCompany paper (Xu et al., 2024 — `arXiv:2412.14161`) is the primary citation; ID from training-data recall (cutoff January 2026); should be verified before external publication. The frontier-completion-rate range (~24-30%) is from the paper's headline result and may be out of date — newer frontier models likely score higher; needs measurement before being cited.

The BA-corpus design is novel — no equivalent currently exists in the public agent-eval literature. The closest analogs are GAIA (`arXiv:2311.12983`) and SWE-bench (`arXiv:2310.06770`), neither of which targets the BA vertical.

## 11. Ratification

This document is **proposed methodology**. It becomes durable Sentinel practice when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- The pre-/post-architectural-change benchmark discipline (§5.1, §5.2).
- The vendor-swap benchmark requirement (§5.3).
- Building `sentinel eval` CLI + corpus + run-history storage.
- Maintaining the R5 boundary on benchmark scores.
- Initial BA-corpus of 50 cases sourced from public corpora before any S/A-tier architectural change ships.
