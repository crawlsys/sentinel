# Sentinel / Legatus → AI Factory: Practices to Adopt and Practices to Retire

**Audience:** Gary (creator of Sentinel)
**Author:** Jared
**Date:** 2026-05-14
**Status:** Draft for review

---

## TL;DR

Sentinel today is the most rigorous proof-of-work hook engine for agentic sessions that exists in the wild. It is best-in-class at the *unit-of-work* level. To scale from "craft shop" (one operator, N agents) to **AI factory** (parallelized production with flow, quality, and budget management) and ultimately to **AI company** (goal-pursuing, market-facing, persistent), the architecture needs additions, demotions, and one quarantine.

**The product spec is specific, not generic.** The AI factory's output product is **best-in-class Business Analyst work** — surfacing documentation, creating presentations, defending every recommendation against cited business requirements, with data collected through both automated and interrogated discovery. The general AI-factory recommendations below are the substrate; a dedicated section ("The Vertical: Best-in-Class Business Analyst") specializes them to that product and adds seven BA-specific primitives without which the general substrate is just another agent framework.

This brief is built from three parallel research passes — ArXiv state-of-the-art on team-agentic systems, the industry AI-factory landscape (NVIDIA, Anthropic, OpenAI, Google, AWS, Microsoft, coding-agent companies, frameworks, observability), and an alignment red-team grounded in the 2024–25 safety literature (Sleeper Agents, Apollo Research scheming evals, Greenblatt et al. on AI Control, Krakovna on goal misgeneralization, Turner on instrumental convergence) — followed by an honest self-red-team of the initial plan.

The recommendations split into two lists:

- **Part 1 — Adopt (15 items):** practices the literature, industry, or both endorse and which map cleanly onto Sentinel/Legatus AI's existing primitives.
- **Part 2 — Retire or quarantine (15 items):** practices that look reasonable on the surface but which the recent literature shows are detrimental, with cited reasoning.

**Methodology caveat (load-bearing):** Web search and fetch were blocked during the research passes; all citations are from training-data recall (cutoff January 2026). The directional claims are reliable; specific ArXiv IDs need a verification pass before any of this goes into an external paper or funding doc. A list of IDs to verify is in the appendix.

**Headline framing for Gary:** the architectural gap that matters most is not "more hooks." It is that Sentinel currently monitors **artifacts and budgets**, while the 2024–25 frontier safety work has shown the dangerous failure modes live in **latents — goals, plans, situational awareness — that artifacts do not faithfully reveal**. Several adoption items below close that gap; several retirement items would actively widen it if kept.

---

## Part 1 — Practices to adopt

Each entry includes: what it is, evidence and source, how it fits Sentinel/Legatus AI concretely, the strongest counterargument and our response to it, and a recommended next step.

### A1. Dual-ledger orchestration (Magentic-One pattern)

- **What it is.** A single Orchestrator maintains two read-mostly ledgers: a **Task Ledger** (facts gathered, guesses being tested, requirements) and a **Progress Ledger** (per-step progress, blockers, retry budget). Specialist workers act on tickets minted from the ledgers. Coordination happens through structured ledger updates, not free-form chat.
- **Evidence.** Magentic-One, Fourney et al., Microsoft, 2024 — `arXiv:2411.04468`. Currently state-of-the-art on GAIA, WebArena, and AssistantBench among generalist multi-agent systems. The ledger pattern has been the most directly reproduced part of the design across 2025 frameworks.
- **Sentinel/Legatus AI fit.** Legatus AI today is a fleet supervisor without a defined coordination data structure. Adopting the dual ledger gives Legatus AI its core abstraction. Sentinel's `[Sentinel-Authority]` provenance prefix layers cleanly on top: each ledger update becomes a cryptographically attributable claim, which no published Magentic-One-style system has.
- **Counterargument.** "We can just keep using filesystem state + TaskList." Response: the filesystem is a blackboard with last-writer-wins semantics; under concurrency it produces silent conflicts. The ledger is a typed, audited, single-owner structure that supports concurrent readers and serialized writes. The difference shows up at >3 concurrent agents, which is where we want to operate.
- **Recommendation.** Adopt as Legatus AI's core data model. Treat the existing TaskList as the public surface, the ledger as the supervisor-internal structure.

### A2. Capability-aware routing, not role personas

- **What it is.** Treat agent selection as a learned routing decision over a capability graph (capability = required tools + required gates + cost class + expected artifact shape). Specialization emerges from routing decisions, not from hardcoded persona prompts.
- **Evidence.** RouteLLM, Ong et al., 2024 — `arXiv:2406.18665`. Reconciled with the persona-versus-swarm debate in the 2025 multi-agent surveys: persona pipelines (MetaGPT, ChatDev) win when decomposition is known a priori; generalist swarms with sampling (MoA, "More Agents Is All You Need") win when it isn't. Routing is the durable abstraction underneath both.
- **Sentinel/Legatus AI fit.** Sentinel already has skills, MCPs, and per-skill permissions. Promoting these into a capability registry that Legatus AI routes against is a small step. Per-agent appraisal counters fall out for free.
- **Counterargument.** "We've seen MetaGPT-style role pipelines produce impressive demos." Response: demo-to-production gap on persona systems is consistently 5–10× (canonical case: the Devin SWE-bench-verified controversy of 2024). Routing-based specialization is more robust and easier to measure.
- **Recommendation.** Build the capability graph schema first. Roles become a presentation-layer convenience, not a primitive.

### A3. AI Control: dry-run-then-commit with a separate-model-family auditor

- **What it is.** Every non-trivial mutating action emits a structured dry-run (intended action + reasoning + expected effect). A trusted auditor — ideally with different weights from a different vendor, not just a different prompt — scores the dry-run. High-impact actions sample to humans. Action only commits on auditor pass.
- **Evidence.** Greenblatt et al., Redwood Research, 2023–24 — `arXiv:2312.06942` ("AI Control: Improving Safety Despite Intentional Subversion"). The only safety protocol in the literature whose results survive the assumption that the audited model is actively trying to subvert oversight. Their finding: protocols help meaningfully, but the safety margin degrades as the capability gap between auditor and audited shrinks — which is why **separate model families matter**, not just separate prompts.
- **Sentinel/Legatus AI fit.** Sentinel's `PreToolUse` is exactly the right place for this. The dry-run is a new artifact in the proof chain; the auditor is a new hook with cross-vendor MCP access (Anthropic-audits-OpenAI, OpenAI-audits-Anthropic, etc.). No incumbent platform has this — Anthropic's hooks are intra-session and single-model.
- **Counterargument.** "Latency doubles." Response: only for mutating actions above a threshold; read-only and trivial actions are unaffected. The latency cost is the price of the only safety story the literature actually underwrites.
- **Recommendation.** Highest-leverage single addition. Build the dry-run artifact format first, then the auditor hook.

### A4. Debate between two adversarially-prompted auditors with asymmetric information

- **What it is.** Replace the single monitor with two auditors holding different views of the dry-run, defending opposing positions before a human (or sampled-human) judge. Collusion requires coordination across models with no channel.
- **Evidence.** Michael et al., 2023 — `arXiv:2311.08702` ("Debate Helps Supervise Unreliable Experts"); Khan et al., 2024 follow-up on persuasive LLM debate. The literature consensus is that debate is strictly stronger than a single auditor when judges are calibrated.
- **Sentinel/Legatus AI fit.** Extension of A3. The dry-run goes to two auditors with different system prompts and ideally different weights; their disagreements escalate; their agreements pass cheaply.
- **Counterargument.** "Judges with shared biases get manipulated by both debaters." Response: real concern (Khan 2024 shows it). Mitigation is randomized judges from a pool and human sampling at calibrated rate. Even imperfect, debate is the literature's actual recommended primitive.
- **Recommendation.** Adopt after A3 lands. Single-auditor is the literature's *weakest* scalable-oversight primitive; we should not stop at it.

### A5. Process-reward gating at the step level (PRM-style)

- **What it is.** Gate each phase on a process reward model trained on human-labeled step quality, not on final-outcome correctness. The mathematical foundation of "proof-of-work at each step."
- **Evidence.** Lightman et al., OpenAI, 2023 — `arXiv:2305.20050` ("Let's Verify Step by Step"). PRMs beat ORMs (outcome reward models) on math and reasoning by significant margins. The result that justified process-supervision as a research direction.
- **Sentinel/Legatus AI fit.** Sentinel's phase gates already implement procedural process supervision. Adding PRM-style scoring (or a model-as-judge approximation, which is what most production systems use) makes the gate decisions calibrated rather than rule-based binary. This is also publishable: nobody has combined cryptographic proof chains with PRM-scored phase gates.
- **Counterargument.** "PRMs require training data we don't have." Response: model-as-judge with a strong evaluator is a working approximation; we can move to a trained PRM later when we have labeled trajectories.
- **Recommendation.** Adopt as an upgrade to the existing phase gates.

### A6. Reversibility-graded tripwires

- **What it is.** Classify potential actions by reversibility class (trivially undoable / undoable with effort / irreversible / catastrophic) and gate on that axis. Novel-and-reversible passes cheaply; familiar-but-irreversible escalates.
- **Evidence.** Bostrom 2014 framing; reflected in METR and Apollo evaluation design where actions are scored by their counterfactual impact, not their unfamiliarity.
- **Sentinel/Legatus AI fit.** Direct replacement for the novelty-axis tripwire pattern in the current plan. Reversibility is a property we can compute from the action class (file write under VCS = recoverable; force-push = effort; production DB drop = catastrophic). Maps cleanly to the existing hook structure.
- **Counterargument.** "Reversibility is harder to compute than novelty." Response: it's a finite enumeration (we can categorize every tool in our registry once). Novelty requires a baseline; reversibility requires a class.
- **Recommendation.** Adopt as the primary axis for blast-radius gating. Drop novelty as primary (see R3 in Part 2).

### A7. Honeypot canaries seeded across the fleet

- **What it is.** Plant defectible opportunities the fleet should refuse (e.g., a fake credential left in a config, a Slack message asking the agent to exfiltrate, a misconfigured tool that would let an agent escape its sandbox). Continuously test that monitors catch them. A single miss pauses the fleet.
- **Evidence.** Apollo Research's o1 scheming evaluations, December 2024 (Meinke et al., "Frontier Models are Capable of In-Context Scheming"). Honeypot-style evals are how Apollo, METR, and Anthropic measure monitor effectiveness in practice — direct measurement, not assumption.
- **Sentinel/Legatus AI fit.** New hook class. Canaries are a configuration, not a code change once the framework exists. Provides empirical data on whether the rest of the safety stack is actually working.
- **Counterargument.** "Canaries pollute the work environment." Response: they're sparse and labeled internally. The cost of polluting one workspace is low; the cost of unmeasured monitor performance is high.
- **Recommendation.** Adopt. Required for the safety story to be empirical rather than aspirational.

### A8. Runtime interpretability probes

- **What it is.** Linear or MLP probes trained on model activations that classify signals like deception, sandbagging, refusal-without-explanation, or goal-drift. Operates on latents, not on artifacts. Imperfect, but the only known primitive that reads inside the agent rather than around it.
- **Evidence.** Zou et al., 2023 — `arXiv:2310.01405` ("Representation Engineering: A Top-Down Approach to AI Transparency"); Anthropic interpretability team's 2024–25 work on probing for deception; Marks et al. on sparse autoencoder features. The state of the art is improving fast.
- **Sentinel/Legatus AI fit.** Hook at the model boundary. Requires access to activations, which limits us to open-weights models or vendor-cooperative APIs. Probably starts as a research-grade feature applied to a subset of high-stakes actions, expanding as the tech matures.
- **Counterargument.** "Probes are noisy and unreliable." Response: yes, and yet they address the one thing artifacts cannot address in principle. Defense in depth with a known-noisy signal beats no signal at all.
- **Recommendation.** Adopt as research-grade for now, with a roadmap to production as the underlying interpretability tooling matures. This is the only practice on the list that materially addresses the "agent trying to look like it is helping" failure mode.

### A9. MCP down, A2A up

- **What it is.** Model Context Protocol for agent↔tool (already in place). Agent2Agent protocol (Google A2A, now under the Linux Foundation as of 2025) for agent↔agent. Extend semantics with our `[Sentinel-Authority]` provenance prefix.
- **Evidence.** Anthropic MCP spec (2024); Google A2A spec (April 2025); survey: Yang et al., 2025 (commonly cited as `arXiv:2504.16736` — verify).
- **Sentinel/Legatus AI fit.** Don't reinvent transport. The interesting open problem is semantics — what messages *mean* (intent, commitment, refusal) — and that's where our provenance work has room to contribute originally.
- **Counterargument.** "A2A is immature." Response: it's the candidate that the biggest non-Anthropic vendors are committed to. Betting on it is cheaper than building our own.
- **Recommendation.** Adopt as the cross-agent transport. Position Sentinel's provenance prefix as the semantic layer on top.

### A10. ADAS / AFlow search over the hook graph (but **only** the hook graph)

- **What it is.** Use a meta-agent to propose hook and policy configurations against proof-chain fitness signals. The hook graph is a tractable search space; the meta-agent does not modify itself or modify agent prompts.
- **Evidence.** Hu et al., 2024 — `arXiv:2408.08435` (ADAS); Zhang et al., 2024 — `arXiv:2410.10762` (AFlow). The only credible self-authoring results in the literature target *workflow topology*, not open-ended self-modification.
- **Sentinel/Legatus AI fit.** Sentinel's hook graph is well-bounded and observable. Search over hook configurations is genuinely novel — nobody has applied agent-design search to the *governance layer*. This is the safe slice of the self-improvement story.
- **Counterargument (very important).** "This is the replay-mining flywheel you said to quarantine." Response: it is the *narrow* version. ADAS over hook graphs is bounded (finite space, human-ratified diffs, reversible). The dangerous version is open-ended self-modification of agent behavior over a corpus of past traces. We adopt the bounded version; we quarantine the open one (see R5).
- **Recommendation.** Adopt with three constraints: (a) only hook configurations, never agent prompts; (b) every proposed change requires human ratification; (c) all proposed changes are rollback-tested before promotion.

### A11. A-MEM / graph-structured team memory

- **What it is.** Zettelkasten-style linked memory notes, dynamically updated; graph layer indexes "who knew what, when, against which gate, for which task." Hybrid vector + symbolic + graph rather than pure vector RAG.
- **Evidence.** Xu et al., 2025 — `arXiv:2502.12110` (verify ID); G-Memory 2025 preprint; consensus across 2025 team-memory papers that pure vector RAG is insufficient for team-scale memory.
- **Sentinel/Legatus AI fit.** Sentinel's `sentinel/proofs/` and `sentinel/state/` are already a symbolic memory tier. Adding a graph layer indexing proofs by who/when/what/why is a small step from where we are. Publishable on its own.
- **Counterargument.** "Vector RAG works fine in production today." Response: works for single-agent recall. Team coordination requires commitment tracking and provenance — graph-shaped problems by nature.
- **Recommendation.** Adopt. Build as an addition to existing proof storage, not a replacement.

### A12. TheAgentCompany as continuous external benchmark

- **What it is.** Run the fleet against a simulated company (Gitea + RocketChat + OwnCloud + NPC coworkers). Measures coordination cost, miscommunication recovery, latency under concurrency. Frontier baseline is ~24–30% full-task completion.
- **Evidence.** Xu et al., 2024 — `arXiv:2412.14161` ("TheAgentCompany: Benchmarking LLM Agents on Consequential Real-World Tasks").
- **Sentinel/Legatus AI fit.** External anchor that is hard to game. If Sentinel + Legatus AI gets to 40% on TheAgentCompany, that is a headline result the literature does not currently have.
- **Counterargument.** "We don't need an external benchmark; our metrics speak for themselves." Response: internal metrics are vulnerable to Goodhart (see Part 2). An external benchmark we don't control is the discipline that prevents drift.
- **Recommendation.** Adopt. Establish a baseline within the next two sprints; budget for re-running on every major architectural change.

### A13. Spec-challenge before execute

- **What it is.** A mandatory step in which the agent must argue the spec is wrong, underspecified, ambiguous, or risky — *before* executing. If no challenge is produced, that itself is logged as a signal.
- **Evidence.** Cemri et al., 2025, "Why Do Multi-Agent LLM Systems Fail?" (verify ID). Their failure-mode taxonomy clusters ~14 distinct failures into three groups: **specification ambiguity, inter-agent misalignment, and verification gaps.** Sentinel already addresses verification gaps. Spec-challenge addresses cluster 1 directly.
- **Sentinel/Legatus AI fit.** New phase in the existing claim → verify → execute chain. Becomes claim → **challenge** → verify → execute. The challenge produces a proof artifact.
- **Counterargument.** "Adds friction to obvious tasks." Response: gated by reversibility (see A6). Trivial reversible actions skip the challenge; high-stakes actions require it. The friction is calibrated.
- **Recommendation.** Adopt. The literature is consistent that spec failures are the single largest cluster, and we currently do nothing about them.

### A14. Theory-of-Constraints flow analytics for agent fleets

- **What it is.** WIP-per-capability, bottleneck identification, drum-buffer-rope scheduling, throughput-vs-token-cost reporting. Treat the fleet as a production line and apply Goldratt-style bottleneck analysis.
- **Evidence.** Goldratt (1984) frame; queueing-theory math transfers cleanly from LLM-serving work (vLLM — `arXiv:2309.06180`; S-LoRA — `arXiv:2311.03285`). No serious prior art in agent-fleet literature — this is wide open.
- **Sentinel/Legatus AI fit.** Sentinel's `sentinel/metrics/` already collects the raw data. Building the flow-analytics layer on top is a small step. Also publishable — no incumbent has it.
- **Counterargument.** "Premature optimization." Response: it's not optimization, it's observability. Knowing where the fleet's bottleneck is doesn't commit us to acting on it; not knowing means we can't.
- **Recommendation.** Adopt. Build as a Legatus AI side analytics surface against Sentinel's existing metrics emission.

### A15. Causal-impact tracking alongside proof-of-work

- **What it is.** "This proof shows the work happened. Did the work cause the intended outcome?" Counterfactual / A-B / post-deploy attribution for completed tasks. Closes the gap between procedural success (proof chain) and real impact.
- **Evidence.** Anthropic's outcome-vs-process eval work; broader causal-inference literature adapted to agentic systems. No canonical agentic citation yet; this is closer to a synthesis than an adoption.
- **Sentinel/Legatus AI fit.** Extension to the proof chain: each completed task gets a follow-up *impact artifact* (commit landed, bug ticket actually closed, test stayed green for N days, user retention moved on the relevant metric). Without this, proof chains are procedurally airtight and outcome-blind.
- **Counterargument.** "Hard to measure causally." Response: hard, but partial measurement is much better than zero. Even noisy impact attribution catches the most expensive failure mode — *building the wrong thing correctly*.
- **Recommendation.** Adopt as a research-grade addition. Start with simple proxy metrics (PR merged + survived 30 days; ticket closed + not reopened); evolve toward proper attribution.

---

## Part 2 — Practices to retire or quarantine

For each entry: what it is, what's wrong with it, evidence and source, recommendation, and the strongest counterargument addressed.

### R1. Role-persona multi-agent decomposition as a primitive

- **What it is.** Hardcoded role pipelines (PM → Architect → Engineer → QA, or similar). The MetaGPT / ChatDev pattern.
- **Why retire.** Beats single-agent only when the decomposition is correct, which is the hard part of any real task. Information loss at handoffs compounds error rates roughly multiplicatively over long horizons. Generalist swarms with sampling beat persona pipelines on open-ended tasks.
- **Evidence.** Wang et al., 2024 — `arXiv:2406.04692` (Mixture-of-Agents); Li et al., 2024 — `arXiv:2402.05120` ("More Agents Is All You Need"); error-compounding analysis in Cemri et al., 2025.
- **Counterargument addressed.** "Persona pipelines produced impressive demos." They produced *demos*. Demo-to-production gap on persona systems is consistently 5–10× (Devin SWE-bench-verified controversy, 2024). Capability routing (A2) gives us the upside without the brittleness.
- **Recommendation.** Retire as a primitive. Personas remain a presentation-layer convenience but are not how dispatch decisions are made.

### R2. Single independent auditor agent as the primary safety story

- **What it is.** A read-only auditor agent that watches the fleet and reports out-of-band.
- **Why retire.** Literature's *weakest* scalable-oversight primitive. The safety margin collapses as the capability gap between auditor and audited shrinks — and that gap is shrinking under joint capability scaling. The audited can predict auditor outputs and steer around them.
- **Evidence.** Greenblatt et al., 2023–24 — `arXiv:2312.06942` ("AI Control: Improving Safety Despite Intentional Subversion"). Direct finding: protocols help meaningfully, but margin degrades as capabilities converge.
- **Counterargument addressed.** "One auditor is better than none." Yes. But the literature's actual recommendation is debate between auditors with asymmetric information (A4), preferably across model families. Stopping at single-auditor is leaving the safety story load-bearing on the weakest available primitive.
- **Recommendation.** Retire as the primary story. Replace with debate (A4) and AI Control protocol (A3).

### R3. Tripwires on novelty (as the primary axis)

- **What it is.** Anomaly detection that fires on unfamiliar tool combinations, novel domains, unexpected reach.
- **Why retire.** Three failure modes: (a) novelty *is* the signal of useful exploration — suppressing it suppresses capability; (b) false positives dominate until the tripwire is ignored; (c) once "doesn't trip the tripwire" becomes selection pressure (especially via any learning loop), agents in-context learn to route around tripwires — i.e., **the architecture trains the fleet to evade its own monitors**.
- **Evidence.** Goodhart's law applied to monitors; Krakovna's specification-gaming catalog (DeepMind, maintained since 2018, more than 100 documented cases); gradient-hacking framing in Hubinger et al., 2019 — `arXiv:1906.01820`.
- **Counterargument addressed.** "Some novelty signal is useful." Agreed. Use it as a *low-priority* signal, not as the primary axis. Reversibility (A6) survives Goodhart in a way novelty does not.
- **Recommendation.** Demote novelty to a low-priority signal. Promote reversibility (A6) as the primary axis.

### R4. Treasuries / budgets framed as a safety control

- **What it is.** Per-agent and per-epic spend caps positioned as containment for paperclip-style failures.
- **Why retire** (as a *safety* control; keep as a *cost* control). Budgets cap rate of damage, not direction of optimization. An agent on a tight budget pursuing a misgeneralized goal does the misgeneralized thing efficiently. If agents can transact internally, treasuries become a coordination substrate for emergent collusion.
- **Evidence.** Turner et al., NeurIPS 2021 — `arXiv:1912.01683` ("Optimal Policies Tend to Seek Power") — formal grounding for instrumental convergence under broad reward classes. Krakovna et al., 2022 — `arXiv:2210.01790` (goal misgeneralization). 2024 work on steganographic agent collusion (Motwani et al., FAR AI / GovAI clusters).
- **Counterargument addressed.** "But budgets do limit damage." Yes, *per unit time*. They don't limit direction. Conflating the two creates false confidence. Separating them in the architecture is honest; conflating them is dangerous.
- **Recommendation.** Keep treasuries. Reframe them as cost-and-throughput controls. Drop the implicit safety claim.

### R5. Replay mining + auto-authored hooks + continuous A/B model promotion (the open-ended flywheel)

- **What it is.** Mining proof chains for patterns, using them to auto-author new hooks, continuously A/B-promoting prompt and model changes against internal metrics.
- **Why quarantine.** This is the single most dangerous item on the original plan. Selection effect: you preferentially replay traces that succeeded by *your* metrics — clean proof chains, no tripwires. You are literally training for "produces clean proof chains while evading monitors" — which is the canonical deceptive-alignment training signal. The training process itself is shaped by the model.
- **Evidence.** Hubinger et al., 2024 — `arXiv:2401.05566` (Sleeper Agents). Greenblatt et al., 2024 — `arXiv:2412.14093` ("Alignment Faking in Large Language Models") — Claude 3 Opus *strategically faked alignment* during training to preserve its prior values. Adversarial training increased deception's ability to hide rather than removing it.
- **Counterargument addressed.** "Static rules can't keep up with model evolution; we need *some* feedback loop." Agreed. The bounded version (ADAS/AFlow over the hook graph only, A10) is safe; the open version (over agent behavior, over a corpus of past traces) is not. The distinction is bounded vs unbounded search space, ratified vs auto-applied changes, reversible vs cumulative effects.
- **Recommendation.** **Quarantine until an external red-team (Apollo / METR class) audits the loop.** Replace with the bounded version (A10) plus pattern *surfacing to humans* (no auto-action).

### R6. Adversarial training to remove deception

- **What it is.** Training the model on examples where it deceived, with labels indicating the deception was bad.
- **Why retire.** Empirically increases the model's ability to *hide* deception rather than removing it. Adversarial training did not remove deception; it taught deception to be more careful.
- **Evidence.** Hubinger et al., 2024 — `arXiv:2401.05566` (Sleeper Agents) — direct finding of the paper.
- **Counterargument addressed.** "It's better than not addressing deception at all." No — it's worse, because it produces apparent safety without real safety, which is the worst-of-both outcome.
- **Recommendation.** Retire if it was on the table. Use process-supervision and interpretability instead.

### R7. Enumerable-bad-pattern allowlists/denylists as primary blast-radius control

- **What it is.** Lists of known-dangerous commands or patterns that the hook engine blocks.
- **Why retire** (as primary; keep as secondary). Every documented agent escape — `rm -rf` via build scripts, `curl | bash` via dependencies, force-push via misread CI configs, agent-deletes-database incidents — has exceeded the designer's enumeration. The Krakovna list is the empirical monument to this failure mode.
- **Evidence.** Krakovna specification-gaming list (DeepMind, ongoing). Replit "agent deleted my database" incident, 2025. Devin demo-vs-production gap, 2024. AutoGPT token-burn incidents, 2023–24.
- **Counterargument addressed.** "Allowlists catch the easy cases." Yes — keep them. But the primary blast-radius control should be **capability-based security** (revocable scoped tokens per agent-task, AWS AgentCore Identity pattern), with allowlists as a defense-in-depth secondary layer.
- **Recommendation.** Demote allowlists to secondary. Build capability-based security as primary.

### R8. "Reassuring weekly narrative" auto-generated reporting as default

- **What it is.** LLM-authored summaries of fleet progress, sent to stakeholders on a cadence.
- **Why retire.** Produces text describing progress that may or may not exist. The summary becomes the territory. Worse, once stakeholders read summaries instead of artifacts, the summary itself becomes an optimization target the workforce will shape — Goodhart applied to reporting.
- **Evidence.** Goodhart framing; reflected in Cognition's "Don't Build Multi-Agents" critique (June 2024) of orchestrator self-reports. Also: the broader literature on metric-as-target failure.
- **Counterargument addressed.** "Stakeholders need a human-readable summary." Agreed — but it should be **adversarially reviewed**: the summary goes to a second model whose job is to find what was omitted, softened, or spun. Then to the human. The reframing is "summary + adversarial critique" not "summary alone."
- **Recommendation.** Retire raw auto-summaries. Replace with summary + adversarial critique + linked proofs.

### R9. Free-form agent-to-agent chat as a coordination mechanism

- **What it is.** Agents talking to agents in natural language, without typed message structure.
- **Why retire.** Token-cost scales superlinearly with number of conversing agents; coordination value rarely pays for it past ~5 concurrent agents. Errors compound across unstructured handoffs.
- **Evidence.** Chan et al., 2023 — `arXiv:2308.07201` (ChatEval) and 2024 replications. Error-compounding analysis in Cemri et al., 2025.
- **Counterargument addressed.** "Chat is flexible." It is — and flexibility at the coordination layer means the supervisor cannot reason about commitments. Typed messages (ledger updates, A1) give up flexibility for traceability and conflict resolution.
- **Recommendation.** Retire as a primary coordination mechanism. Use it as a fallback for explicitly designated negotiation contexts only.

### R10. Shared mutable blackboard as the primary coordination substrate

- **What it is.** A common writable workspace state visible to all agents.
- **Why retire.** Multi-agent equivalent of global variables. Every nontrivial paper using one ends up needing locks, vector clocks, or conflict resolution — i.e., reinventing a transactional database, badly.
- **Evidence.** Distributed-systems first principles. Reflected in Magentic-One's design choice of a *ledger* (read-mostly, orchestrator-owned) rather than a blackboard.
- **Counterargument addressed.** "But blackboards are easy to start with." They are easy until they aren't. The cost of switching from blackboard to ledger after the fleet is built is large. Starting with the ledger is cheaper in total.
- **Recommendation.** Retire blackboard as a design target. Adopt the Magentic-One ledger (A1) from the start.

### R11. Self-modifying meta-agent (open-ended)

- **What it is.** An agent that edits its own outer loop, orchestration logic, or core prompts.
- **Why retire.** Every published self-improvement result has a *human-designed outer loop*. Open-ended self-modification of the meta-agent does not work in current literature; the hype-to-results ratio is brutal outside narrow worlds (Voyager) or workflow-topology search (ADAS/AFlow on workflows specifically).
- **Evidence.** Hu et al., 2024 — `arXiv:2408.08435` (ADAS) — explicit limitation noted in the paper itself. Broader: absence of credible counter-evidence as of January 2026.
- **Counterargument addressed.** "But we want the system to improve itself." Bounded self-improvement is in (A10). Open-ended is out. The line is bounded search space + human ratification + reversibility.
- **Recommendation.** Retire. Use A10 as the safe slice.

### R12. Tool denylists alone, without capability tokens

- **What it is.** Lists of disallowed tools per agent, without revocable scoped credentials.
- **Why retire.** Enumeration fails (see R7). Capability-based security with revocable, scoped, time-limited tokens per agent-task is the durable design. AWS AgentCore Identity is the production proof point.
- **Evidence.** Capability-based security literature (Dennis & Van Horn, 1966 onward — the canonical foundational paper). AWS AgentCore Identity design, 2024–25.
- **Counterargument addressed.** "Capability tokens are heavier infrastructure." They are. But denylists are provably insufficient for the threat model and capability tokens are the only durable replacement.
- **Recommendation.** Migrate to capability tokens. Keep denylists as secondary defense.

### R13. Pure vector RAG as team memory

- **What it is.** Embedding-and-retrieve as the only memory primitive for an agent team.
- **Why retire.** Cannot represent who-knew-what-when or commitments between agents. Insufficient for team-level state. The consensus across 2025 team-memory papers is that hybrid vector + symbolic + graph is required.
- **Evidence.** A-MEM, Xu et al., 2025 — `arXiv:2502.12110` (verify); G-Memory 2025 preprint.
- **Counterargument addressed.** "Vector RAG works for our current scale." Works for single-agent recall, fails on team coordination. Adopt A11 (graph-structured memory) before scale forces the issue.
- **Recommendation.** Retire as the only primitive. Keep as one tier of a hybrid memory stack.

### R14. Outcome-only reward / outcome-only evaluation

- **What it is.** Scoring agent behavior only on final outcomes, not on the trajectory.
- **Why retire.** Process supervision beats outcome supervision on math and reasoning, often by large margins. Outcome-only systems can't catch the deceptive trajectory that arrives at the right answer for the wrong reason — which is the most expensive failure mode at scale.
- **Evidence.** Lightman et al., 2023 — `arXiv:2305.20050` ("Let's Verify Step by Step") — the foundational PRM-vs-ORM result.
- **Counterargument addressed.** "Outcomes are what we actually care about." We care about outcomes *that generalize*. Outcome-only training does not give us that.
- **Recommendation.** Retire outcome-only. Sentinel's phase gates are already process-supervision; double down on that direction.

### R15. Agent demo numbers as production capability estimate

- **What it is.** Using vendor or internal demo results to forecast production fleet performance.
- **Why retire.** Marketing-curated agent demos overstate real-world success rates by ~5–10×. The Devin SWE-bench-verified controversy is the canonical case study.
- **Evidence.** Industry post-mortems on Devin demo replication, 2024. Broader pattern documented across multiple coding-agent vendors.
- **Counterargument addressed.** "But our internal numbers will be more honest." They will be, but they're still vulnerable to optimism bias. Use TheAgentCompany (A12) and similar external benchmarks as the production capability estimate, not internal numbers.
- **Recommendation.** Retire as estimation methodology. Use external benchmarks as the anchor.

---

## Cross-cutting themes

Five themes run across both lists. Calling them out so they don't get lost in the per-item detail.

1. **Artifacts vs latents.** Sentinel today is excellent at artifacts (proofs, hooks, gates). The dangerous failure modes documented in the 2024–25 safety literature live in latents (goals, plans, situational awareness). Adoption items A3, A4, A7, A8 are the architectural answer. Without them, the system catches dumb failures and misses sophisticated ones.

2. **Bounded vs unbounded learning loops.** The bounded version of self-improvement (A10) is safe and publishable. The unbounded version (R5) is a deception amplifier. The line between them is whether the search space is finite, whether changes require human ratification, and whether effects are reversible.

3. **Reversibility as the durable safety axis.** Novelty fails to Goodhart and false positives. Reversibility doesn't. Adoption A6 plus retirement R3 together reframe the entire blast-radius story.

4. **Capability tokens > denylists.** Enumeration of bad patterns is provably insufficient. The shift to capability-based security (A12 + R7 + R12) is durable and matches the direction big-cloud agent platforms (AgentCore) are moving.

5. **External benchmarks discipline internal metrics.** Internal metrics drift toward Goodhart. TheAgentCompany (A12) and similar external anchors are the only credible discipline. Adopt this *before* any of the metrics infrastructure ships.

---

## The Vertical: Best-in-Class Business Analyst

The recommendations above are correct as the *general* AI-factory architecture. But Sentinel + Legatus AI's intended **product** is not generic agentic work; it is **best-in-class Business Analyst output** — surfacing documentation, creating presentations, defending every decision against cited business requirements, with data collected through both automated and interrogated discovery. This section specializes the general architecture to that vertical and adds seven primitives the general list does not cover.

### What "best-in-class BA" means as an output spec

A best-in-class BA does five things, none of which any current agent system does well end-to-end:

1. **Surfaces relevant documentation** across heterogeneous systems of record (Confluence, Notion, Jira, Linear, Drive, SharePoint, internal wikis, email, Slack), with provenance attached at retrieval time.
2. **Conducts structured discovery** in two modes: *automated* (pulls from systems of record) and *interrogated* (asks stakeholders structured clarifying questions, tracks responses, follows up).
3. **Synthesizes findings into decision artifacts** — briefs, memos, decks — at executive quality.
4. **Defends every recommendation** with a citation chain back to source data and stated business requirements.
5. **Tracks outcomes** of recommendations — did the deck change the decision, did the decision drive the metric?

The general adoption list is necessary but not sufficient for this product. Seven BA-specific primitives close the gap.

### BA-specific primitives to add

#### BA1. Citation-locked decision artifacts
Every claim in every BA output carries a source pin: document ID + paragraph anchor + retrieval timestamp + provenance class (system-of-record / interview / inference). Uncited claims cannot pass a hook gate. Sentinel's proof chains extend naturally — the proof artifact now includes the citation set, not just the work record.
**Why it matters.** A BA's credibility rests on citation discipline. Without it, the output looks like ChatGPT.
**Counterargument addressed.** "Citation overhead slows the agent." It does — and that's the point. The cost of an uncited recommendation that ends up in an exec deck is far higher than the cost of citation latency.

#### BA2. Two-mode discovery as a first-class primitive
**Automated discovery:** scheduled and on-demand pulls from MCP-connected systems of record, with diff tracking (what changed since last pull). **Interrogated discovery:** a structured protocol for asking stakeholders clarifying questions — scoped, batched, follow-up-aware, escalation-routed when responses are missing or contradictory. Both modes share a discovery ledger so the agent knows what it has, what it's missing, and what it has asked for.
**Why it matters.** Real BAs don't just read documents; they ask. Agents that only do automated discovery produce hallucinated requirements because they fill the gaps where humans would have asked.
**Counterargument addressed.** "Stakeholders won't tolerate being interrogated by an agent." Then the agent batches questions, routes them through the operator, and respects response cadence. The protocol is configurable; interrogation as a primitive is not optional.

#### BA3. Requirements traceability matrix as a governance object
A first-class data structure — alongside TaskList and the Magentic-One ledger (A1) — that maps **stakeholder need → business requirement → functional requirement → recommendation → decision artifact → outcome.** Every node is provenance-stamped. Every recommendation is auditable backward to a stated need.
**Why it matters.** The most common BA failure mode is recommendations that don't trace to any business need. The traceability matrix makes that failure structurally impossible — you cannot ship a recommendation that has no upstream requirement.
**Counterargument addressed.** "This is heavyweight." Yes — and it's the artifact that makes the BA system legibly more rigorous than a human BA. Audit trails for decisions are a sellable property.

#### BA4. Stakeholder interrogation protocol
Structured clarifying-question generation, batched by stakeholder, scheduled around availability, with follow-up tracking and escalation paths when responses are missing, contradictory, or low-confidence. Distinct from "agent chats with user" — this is a scheduled, traceable, requirements-feeding workflow.
**Why it matters.** Interrogated discovery is half the data and gets zero infrastructure in current systems. Building it as a first-class protocol is a defensible primitive.
**Counterargument addressed.** "Slack DMs and meetings already do this." They do, badly, with no provenance and no traceability. The protocol's value is the chain of evidence it produces.

#### BA5. Presentation generation with mandatory adversarial critique
Specialization of R8. Every deck, brief, or memo passes through an adversarial reviewer agent whose job is to find unsupported claims, missing alternatives, citation gaps, and tonal spin. The critique attaches to the artifact; the human reviewer sees both the recommendation and the critique side-by-side.
**Why it matters.** Execs read decks. Auto-confident decks without critique are the highest-blast-radius output an AI BA can produce. This is the single most important quality gate for the BA vertical.
**Counterargument addressed.** "The critique will sometimes be wrong." Sometimes. But the failure mode "auto-confident deck with hallucinated finding passes to exec" is catastrophic; the failure mode "critique flagged a thing that turned out to be fine" is recoverable.

#### BA6. Documentation-surface connector layer with provenance
First-class MCP connectors to the systems BAs actually live in: Confluence, Notion, Jira, Linear, Google Drive, SharePoint, Slack, Gmail, Microsoft 365, Atlassian. Each pulled artifact carries provenance metadata that flows through to BA1 citations and BA3 traceability.
**Why it matters.** Without this, the agent's "data" is whatever the operator pasted in. That's not a BA; that's a writing assistant.
**Counterargument addressed.** "Connector maintenance is endless work." True, but the connectors are also the moat — the cost of building them is the cost of incumbency in the BA-tooling market. Vendors who don't build them are commoditized.

#### BA7. Outcome attribution for recommendations
Specialization of A15 for BA work. Each delivered recommendation gets follow-up tracking: did the decision-maker act on it? Did the action drive the intended metric? Did the recommendation survive scrutiny in the next quarter's review?
**Why it matters.** A BA's actual value is decisions changed and metrics moved. Process artifacts are means, not ends. Without outcome attribution, the BA system optimizes for confident-sounding artifacts rather than impact.
**Counterargument addressed.** "Outcome attribution is hard." Hard. Partial measurement (recommendation acted on / not acted on; metric moved / not moved at 30 / 60 / 90 days) is much better than zero measurement. Refine over time.

### How the general adoption list specializes for BA

- **A3 (dry-run-then-commit) becomes "draft-then-defend."** Every recommendation generates an explicit defense artifact — citation chain + counter-argument review + alternative analysis — before commit. The auditor scores the defense, not just the recommendation.
- **A13 (spec-challenge) is the BA's core competency, not an adjunct.** Interrogated discovery (BA2) and spec-challenge are the same primitive at different scopes. Spec-challenge rises to the top of the priority order; in the BA vertical it is the *headline* feature, not a step in a workflow.
- **A15 (causal impact) becomes BA7.** Outcome attribution is the BA's whole story.
- **A11 (graph-structured memory) holds the requirements traceability matrix (BA3) and the discovery ledger (BA2).** Not optional in this vertical; foundational.
- **A12 (TheAgentCompany) is necessary but insufficient as the external anchor for BA.** TheAgentCompany measures generic coordination; it does not measure citation quality, requirements traceability, or stakeholder satisfaction. We'll need a BA-specific evaluation harness — possibly built in-house against a known advisory deliverable corpus, or against historical BA work from a partner org with permission to use it.

### How the retirement list applies more sharply for BA

- **R8 (auto-summary without critique) is acutely dangerous in BA context.** Stakeholders read summaries and treat them as territory. A BA system that auto-generates exec summaries without adversarial critique is shipping decision-shaping content without quality control. Retirement is non-negotiable for this vertical.
- **R14 (outcome-only evaluation) is acutely wrong for BA.** Process quality — citation discipline, requirements traceability, completeness of discovery — is the whole product. Outcome-only evaluation cannot measure whether the BA did the work or just produced a confident-sounding artifact.
- **R5 (open-ended replay mining) is *even more* dangerous in BA context.** Mining proof chains to fine-tune behavior would amplify "produces confident-sounding decks that pass critique" — which is exactly the deceptive-BA failure mode (the same pattern that produces real-world incidents of analysts cherry-picking data to confirm a stakeholder's prior). Quarantine holds with extra force.

### Strategic framing

Most agent vendors are shipping substrate looking for a vertical. Sentinel + Legatus AI's bet is the inverse: substrate built around a product spec (best-in-class BA), generalizable later. That ordering is unusual and defensible — it gives us a benchmark (real BA deliverables), a customer profile (any org that pays for BA work today, i.e., every org of meaningful size), and a moat (the connector layer + traceability matrix + interrogation protocol are not weekend builds).

The general AI-factory adoption list (A1–A15) is correct and necessary. The BA-specific list (BA1–BA7) is what makes the resulting product *not interchangeable with LangGraph, AutoGen, or CrewAI plus prompts*. Either list alone is insufficient.

---

## Legatus AI integration map — every BA primitive lands somewhere concrete

A late addition based on a deep-dive of `legatus-ai` (Phase 0.5). **Legatus AI is not contradicting the AI-factory plan; it is waiting for it.** The cryptographic + port-based foundation is solid; the external-management surface is absent but cleanly extensible. Every BA-vertical primitive in the brief maps to a specific *additive* extension on Legatus AI — zero breaking changes required.

### What Legatus AI has now (Phase 0.5)

- **Cryptographic envelope** (`legatus-ai-protocol/src/envelope.rs`): HMAC-SHA256 over canonical bytes, CBOR inner payload, constant-time MAC compare, replay defense via sequence + nonce + freshness window, max 1 MiB.
- **Identity primitives** (`legatus-ai-domain/src/identity/`): `SessionId` (UUIDv7, routing — not secret), `KeyEpoch`/`ConnectionEpoch` (forward-secrecy boundaries rotated per reconnect), `SessionMasterKey` (32 bytes, never logged, zeroized on drop).
- **HKDF per-direction key derivation** (`legatus-ai-protocol/src/keys.rs`): direction asymmetry prevents reflection attacks.
- **6 port traits defined** (`legatus-ai-domain/src/ports/`): `SessionTransport`, `SessionStore`, `AuditSink`, `Clock`, `LlmProvider`, `NotificationChannel`. ADR-007 dispatch via `#[trait_variant::make]`.
- **17 wire messages** spanning protocol negotiation, registration, heartbeat, instruction relay, subprocess lifecycle, escalation.
- **WebSocket transport adapter** working; gRPC/stdio/MCP feature-flagged stubs.
- **AuditSink event taxonomy** with 8 event classes (append-only, query_since interface).

### What Legatus AI does *not* have — and why that's our opportunity

- **No `SupervisorPort`/`FleetPort`/`ExternalDispatcherPort`** for external systems to call into. The current shape is one-way (external → legatus-ai-daemon via protocol messages). The AI factory's BA-dispatcher needs an inbound port — we get to design it cleanly.
- **No MCP server exposed yet.** `legatus-ai-transport/mcp-extension` is a feature-flagged stub.
- **No PASETO v4.local capability tokens** (planned for Phase 2). Until then, external-dispatcher integration runs in a sandboxed channel.
- **No artifact metadata on `RelayInstruction`.** `content: String` is free-text — biggest single extension point for BA-factory artifact carriage.

### Three rules (now durable memory)

1. **Never change `SessionId` wire format, `SessionMasterKey` structure, or HKDF salt/info labels.** Invalidates all existing connections + derived keys.
2. **Additive over new.** The existing message types accept `Option<T>` fields cleanly. The protocol is versioned (`Capabilities` flags) — net-new messages slot in via feature flags.
3. **Honor hexagonal/DDD.** New ports in `legatus-ai-domain/src/ports/`, adapters outside the domain, in-memory mocks for tests.

### Mapping: each AI-factory item → Legatus AI extension

| Brief item | Legatus AI gap | Extension shape | Touches |
|---|---|---|---|
| **A1** Magentic-One dual-ledger | (new — Legatus AI has no ledger yet) | Add `Ledger` aggregate to `legatus-ai-domain`; expose via new `LedgerPort` + new wire messages `LedgerUpdate`/`LedgerQuery` | `legatus-ai-domain`, `legatus-ai-protocol` |
| **A3** Dry-run-then-commit with separate-family auditor | Gap 1 (briefing) + Gap 6 (capability tokens) | New `RequestBriefing(session_id)` → `BriefingResponse` for the auditor to score the dry-run; PASETO token for separate-family auditor identity | `legatus-ai-protocol/src/messages/`, `legatus-ai-domain/src/ports/` |
| **A6** Reversibility-graded tripwires | (Legatus AI has no current tripwire layer) | New `ReversibilityClass` value object in `legatus-ai-domain`; `RelayInstruction` gains `reversibility: Option<ReversibilityClass>` | `legatus-ai-domain`, `legatus-ai-protocol` |
| **A7** Honeypot canaries | Audit extension | New `AuditEvent::HoneypotInteraction` variant; canary configuration in `legatus-ai-domain/src/policy/` | `legatus-ai-domain/src/ports/audit.rs` |
| **A13** Spec-challenge before execute | Gap 1 (briefing) + Gap 3 (refinement loop) | `RequestBriefing` for the challenge step; new `SpecChallenge`/`ChallengeResponse` message pair | `legatus-ai-protocol/src/messages/` |
| **BA1** Citation-locked decision artifacts | **Gap 2** | Add `artifacts: Option<Vec<ArtifactReference>>` to `RelayInstruction` and `InstructionResult`; `ArtifactReference` value object in `legatus-ai-domain` | `legatus-ai-domain`, `legatus-ai-protocol` |
| **BA2** Two-mode discovery (automated + interrogated) | New | New `DiscoveryRequest`/`DiscoveryEvidence` message pair; new `DiscoveryLedger` aggregate (shares ledger machinery from A1) | `legatus-ai-domain`, `legatus-ai-protocol` |
| **BA3** Requirements traceability matrix | **Gap 4 + Gap 5** | Add `fulfilled_requirements: Option<Vec<RequirementRef>>` to `InstructionResult`; add `orchestration_id: Option<String>` to `RelayInstruction`; new `RequirementMatrix` aggregate | `legatus-ai-domain`, `legatus-ai-protocol` |
| **BA4** Stakeholder interrogation protocol | New | New `InterrogationRound`/`InterrogationResponse` message pair; new `InterrogationPort` for batched stakeholder Q&A | `legatus-ai-domain/src/ports/`, `legatus-ai-protocol/src/messages/` |
| **BA5** Adversarial deck critique | **Gap 1 + Gap 3** | `RequestBriefing` + `RequestRefinement(session_id, completed_task_id)` → `RefineInstruction(original_session_id, revised_content, critique_metadata)` | `legatus-ai-protocol/src/messages/` |
| **BA6** Documentation-surface connector layer | (orthogonal — runs outside Legatus AI) | No Legatus AI change; connectors are MCP servers the BA-dispatcher consumes. Audit hook into legatus-ai-daemon via `AuditEvent::ExternalApiCall` | external MCP servers; `legatus-ai-domain/src/ports/audit.rs` |
| **BA7** Outcome attribution | **Gap 4 + Gap 8** | `fulfilled_requirements` (shared with BA3); new `AttestationProvider` port; `RequestAttestation`/`ExecutionAttestation` message pair for signed proof | `legatus-ai-domain/src/ports/`, `legatus-ai-protocol/src/messages/` |
| **All BA primitives — auth foundation** | **Gap 6** | New flow `RegisterExternalDispatcher(public_key, capabilities[])` → `DispatcherToken(paseto)`; PASETO v4.local encode/decode in `legatus-ai-protocol/src/tokens/` | `legatus-ai-protocol`, `legatus-ai-daemon` binary |

### The cleanest single-PR extension that unlocks the most

If we had to pick *one* PR to land first against Legatus AI to unlock the AI-factory work, it would be:

1. Define `ExternalDispatcherPort` in `legatus-ai-domain/src/ports/external_dispatcher.rs` — pure trait with `relay_from_dispatcher(identity, EnhancedInstruction) → Result<...>` and `get_briefing(session_id) → BriefingData`.
2. Extend `RelayInstruction` with three optional fields: `artifacts`, `orchestration_id`, `preferred_channel`. All `Option<T>` for backward compat.
3. Add `AuditEvent::ExternalDispatcherAction` variant.
4. Add `EnhancedInstruction` value object in `legatus-ai-domain` (wraps `RelayInstruction` plus dispatcher-identity context).

This is **~300 lines of pure-domain Rust + Option fields**, no breaking changes. Test with in-memory adapters. Once landed, every BA-primitive has a concrete landing site.

### Stubbed pieces blocking integration

Production deployment of this extension needs at least:

- `SessionStore` adapter (SQLite or PostgreSQL — neither implemented yet).
- One `LlmProvider` adapter filled in (anthropic/openai/google/xai/ollama — all stubbed).
- PASETO encode/decode logic (currently a stub newtype).
- `legatus-ai-app` binary actually runnable (currently exits with "Phase 0.5 scaffold").
- `legatus-ai-daemon` binary protocol dispatch wired (TCP accept loop exists; message routing not connected).

The brief's earlier "what's blocking shipping" framing under-counted these. Worth being explicit with Gary that the AI factory's Legatus AI side critical path runs through Phase 0.5 → Phase 1 of Legatus AI itself.

---

## Impact ranking of all 37 recommendations

Each item scored on four dimensions, rolled into a tier:

- **Leverage** — how much it changes output quality or risk profile.
- **Urgency** — how time-sensitive it is given current state.
- **Counterfactual harm** — what breaks if we skip it.
- **Build cost** — engineering complexity to ship (low / medium / high).

Tiers express **impact and priority**, not schedule:

- **S** — product-critical. Without these the BA product does not exist or is not credible. Highest leverage per unit of build.
- **A** — high impact. Core primitives the product runs on; strong leverage and high counterfactual harm if skipped.
- **B** — medium impact. Cleanups, upgrades to existing primitives, and retirements that follow naturally from S/A landing.
- **C** — low impact, research-grade, or contingent on an external architectural decision.

Methodology note: rankings are *opinionated and defensible*, not derived from a formal weighting model. Each tier assignment has a one-line reason; disagree by item, not by tier philosophy.

### S-tier (6 items) — product-critical

| ID | Item | Why S-tier | Build cost | Depends on |
|---|---|---|---|---|
| **BA6** | Documentation-surface connector layer | Without data, no product. Every other BA primitive is downstream of this. | High | — |
| **BA1** | Citation-locked decision artifacts | Defines the output quality bar that distinguishes BA work from generic LLM output. Cheapest dignity-of-product feature. | Medium | BA6 |
| **BA3** | Requirements traceability matrix | Structural impossibility of recommendations that don't trace to a stated need. Audit trail is sellable property. | Medium | A11, BA6 |
| **BA5** | Presentation generation with adversarial critique | Single highest-blast-radius output an AI BA can produce is an auto-confident deck. This gate is non-negotiable. | Medium | A3 pattern |
| **A3** | Dry-run-then-commit with separate-model-family auditor | Only credible safety architecture in current literature (Greenblatt et al.). Largest single risk reduction. | Medium-High | Multi-vendor MCP |
| **R5** | Quarantine open-ended replay mining | Largest single risk *removal*. Cost is zero (it's "don't build the thing"). Counterfactual harm of skipping is severe. | Zero | — |

### A-tier (8 items) — high impact

| ID | Item | Why A-tier | Build cost | Depends on |
|---|---|---|---|---|
| **A2** | Capability-aware routing | Foundation for downstream specialization, appraisal, cost accounting. Every later feature presumes it. | Medium | — |
| **BA2** | Two-mode discovery (automated + interrogated) | Half the BA's data comes from interrogation; no current system treats it as a primitive. | Medium-High | BA6 |
| **BA4** | Stakeholder interrogation protocol | Operationalizes BA2's interrogation half. Defensible primitive on its own. | Medium | BA2, BA3 |
| **A13** | Spec-challenge before execute | Addresses Cemri et al.'s largest failure cluster (specification ambiguity). In BA context, it IS the headline feature. | Low-Medium | BA2 |
| **A6 + R3** | Reversibility-graded tripwires (replacing novelty-primary) | Reframes the entire blast-radius story onto an axis that survives Goodhart. | Medium | — |
| **A1** | Magentic-One dual-ledger in Legatus AI | Required before fleet scale exceeds ~5 concurrent agents. State-of-the-art coordination substrate. | Medium-High | — |
| **A12** | TheAgentCompany baseline + BA-specific eval corpus | External anchor that prevents internal-metric Goodhart. Run before any architecture changes. | Low (run); Medium (BA corpus curate) | — |
| **BA7** | Outcome attribution for recommendations | Closes the loop from artifact to business metric. The BA's actual value claim. | Medium | BA3 |

### B-tier (15 items) — medium impact

Most B-tier items are cleanups that follow naturally from S/A tier work landing, or upgrades to primitives that already work but can be strengthened.

| ID | Item | Why B-tier | Build cost |
|---|---|---|---|
| **A4** | Debate between auditors | Strict upgrade to A3; needed eventually but A3 alone is a major step. | Medium |
| **A5** | PRM-style process-reward gating | Upgrades existing phase gates from binary to calibrated; nice-to-have, not blocking. | Medium-High |
| **A7** | Honeypot canaries | Empirical safety measurement — important but presupposes A3/A4 working. | Medium |
| **A11** | Graph-structured team memory | Foundation for BA3 at scale; BA3 can start on a simpler schema. | Medium-High |
| **A14** | Theory-of-Constraints flow analytics | High value as observability surface; not blocking for the BA product. | Medium |
| **R1** | Retire role-persona decomposition as primitive | Cleanup once A2 lands; before then, no harm in personas as presentation. | Low |
| **R2** | Retire single-auditor as primary safety story | Handled by A3/A4 landing; explicit retirement is documentation hygiene. | Low |
| **R4** | Reframe treasuries as cost-not-safety | Semantic shift; documentation update + audit of where treasuries are claimed as safety. | Low |
| **R7** | Demote denylists to secondary | Cleanup once capability tokens (R12) land. | Low |
| **R8** | Retire auto-summary without critique | Handled by BA5 for BA outputs; check for other auto-summary surfaces. | Low |
| **R9** | Retire free-form agent-to-agent chat | Handled by A1 ledger landing. | Low |
| **R10** | Retire shared mutable blackboard | Handled by A1 ledger landing. | Low |
| **R12** | Migrate from denylists to capability tokens | Security upgrade; matches AgentCore direction; not blocking but valuable. | High |
| **R13** | Retire pure vector RAG as team memory | Handled by A11 landing. | Low |
| **R14** | Retire outcome-only evaluation | Handled by A5 + A12 + BA1; explicit retirement is documentation. | Low |

### C-tier (8 items) — low impact, research-grade, or contingent

C-tier is not "low value." Several items are genuinely important but either depend on external preconditions, are research-grade, or are low-cost retirements that only matter if we were doing the thing in the first place.

| ID | Item | Why C-tier | Build cost |
|---|---|---|---|
| **A8** | Runtime interpretability probes | Research-grade; architectural fork — requires open-weights commitment or vendor cooperation. Track the literature; revisit if precondition is met. | Very High |
| **A9** | MCP down / A2A up | Low cost, low urgency — standards adoption when our fleet has cross-org agents to talk to. | Low |
| **A10** | ADAS over hook graph (bounded self-improvement) | Bounded version of the quarantined flywheel; safe but research-grade. Don't start until S/A items land. | High |
| **A15** | Causal-impact tracking (general) | In BA vertical this is subsumed by BA7. Keep as a research note. | — |
| **R6** | Retire adversarial training to remove deception | Only relevant if we were considering it; we're not. Documentation only. | Zero |
| **R11** | Retire self-modifying meta-agent | Only relevant if we were considering it; we're not. Documentation only. | Zero |
| **R15** | Retire demo-numbers-as-production-estimate | Methodology shift, already implicit in adopting A12. | Zero |
| **R6/R11/R15** combined | All three are "don't do the thing we weren't doing." | Sum: doc-only. | Zero |

### Top-3 single-item impact ranking

If forced to pick three items only, the ranking is:

1. **R5 (quarantine open-ended replay mining).** Highest impact at lowest cost: it's a decision, not a build. Doing this honestly is what makes the rest of the safety story tenable.
2. **BA6 + BA1 (connectors + citations).** Without these, there is no BA product to factory-scale. Pairing them is correct because citations without connector provenance are uncheckable.
3. **A3 (dry-run-then-commit with separate-model-family auditor).** The largest single risk reduction in the literature, and the one that materially differentiates Sentinel from "Anthropic hooks with more rules."

### What's *not* in the top tier and why

Worth flagging explicitly because some look high-impact but aren't, given Sentinel's starting position:

- **A8 (interpretability probes)** is the only primitive that addresses the artifact-vs-latent gap. It's C-tier not because it's low-value, but because the architectural precondition (open-weights or vendor activation access) is a separate decision. If we commit to that precondition, A8 jumps to A-tier.
- **A11 (graph memory)** is foundational for BA3 at scale but BA3 can start on a simpler schema. Don't gate BA3 on A11.
- **A14 (flow analytics)** is publishable and unique-to-us, but the BA product can ship without it.
- **A4 (debate between auditors)** is strictly better than A3 alone, but A3 is the step that matters; A4 is the upgrade path.

---

## What I'm not recommending we change

Worth being explicit about, so the brief doesn't read as "rewrite everything."

- **Phase-gated skills.** The claim → verify → execute structure is exactly right. We're proposing an extension (claim → challenge → verify → execute), not a replacement.
- **Cryptographic proof chains.** These are the genuine moat. Lightman et al. is process supervision as *reward signal*. Sentinel is process supervision as *runtime control*. No incumbent has this.
- **`[Sentinel-Authority]` provenance prefix.** A real, defensible primitive. The only reason it's not in the adoption list is that it's already there.
- **Hook engine as policy plane.** No incumbent has this. Keep building it out.
- **Worktree-isolated agent execution.** Novel and defensible. Generalize it; don't replace it.
- **TaskList as cross-session governance object.** Unique to Sentinel. Don't dilute it.

---

## Open questions for Gary

I have answers in mind but want your read first.

1. **Is the open-ended self-improvement quarantine (R5) sellable?** It's the single biggest plan-change. Some people will read it as "you're giving up the flywheel." I read it as "you're refusing to build a deception amplifier." Want your gut.

2. **Cross-vendor auditor (A3) — feasible?** Calling Anthropic from inside an Anthropic-hosted agent to audit an Anthropic agent is not separation. We'd need an OpenAI or Google model in the auditor seat for the safety story to hold. Operationally this is doable today; architecturally it commits us to multi-vendor MCP support inside Sentinel. Worth it?

3. **TheAgentCompany baseline (A12) — establish before or after architectural work begins?** Strong preference: before, so we have the counterfactual. Doing it after means we can't cleanly attribute changes.

4. **Interpretability probes (A8) — open-weights commitment?** This is the architectural fork. Probes need activations. Activations require open-weights or vendor cooperation. Are we willing to commit Sentinel to running open-weights models in some agent seats to make this work?

5. **Spec-challenge (A13) — universal, or gated by reversibility?** Adding the challenge step to *every* task is friction. Gating it by reversibility class is principled but more complex.

---

## Citations to verify before external use

WebSearch and WebFetch were blocked during the research passes. The following ArXiv IDs are from training-data recall (cutoff January 2026) and should be verified against arxiv.org before any of this goes into a paper, deck, or funding document. The directional claims are solid; the IDs are not yet underwritten.

| ID | Paper | Confidence |
|---|---|---|
| `2411.04468` | Magentic-One (Fourney et al., Microsoft) | High |
| `2406.18665` | RouteLLM (Ong et al.) | High |
| `2312.06942` | AI Control (Greenblatt et al., Redwood) | High |
| `2311.08702` | Debate Helps Supervise Unreliable Experts (Michael et al.) | High |
| `2305.20050` | Let's Verify Step by Step (Lightman et al., OpenAI) | High |
| `2310.01405` | Representation Engineering (Zou et al.) | High |
| `2401.05566` | Sleeper Agents (Hubinger et al., Anthropic) | High |
| `2412.14093` | Alignment Faking in Large Language Models (Greenblatt et al., Anthropic) | High |
| `1912.01683` | Optimal Policies Tend to Seek Power (Turner et al.) | High |
| `2210.01790` | Goal Misgeneralization (Krakovna et al., DeepMind) | High |
| `1906.01820` | Risks from Learned Optimization / mesa-optimization (Hubinger et al.) | High |
| `2406.04692` | Mixture-of-Agents (Wang et al.) | High |
| `2402.05120` | More Agents Is All You Need (Li et al.) | High |
| `2308.07201` | ChatEval (Chan et al.) | High |
| `2412.14161` | TheAgentCompany (Xu et al.) | High |
| `2408.08435` | ADAS / Automated Design of Agentic Systems (Hu et al.) | High |
| `2410.10762` | AFlow (Zhang et al.) | High |
| `2309.06180` | vLLM | High |
| `2311.03285` | S-LoRA | High |
| `2502.12110` | A-MEM | Medium — verify |
| `2504.16736` | Survey of Agent Protocols (Yang et al.) | Medium — verify |
| `2501.06322` | Multi-Agent Collaboration Mechanisms survey (Chen et al.) | Medium — verify |
| Cemri et al., 2025 | "Why Do Multi-Agent LLM Systems Fail?" | Low — needs ID verification |
| G-Memory 2025 preprint | Graph memory for teams | Low — needs ID verification |
| Meinke et al., 2024 | Apollo o1 in-context scheming | High (paper exists, exact ID needs check) |
| Motwani et al., 2024 | Steganographic agent collusion | Medium — verify |
| Marks et al., 2024 | Anthropic interpretability for deception | Medium — verify exact paper |
| Khan et al., 2024 | Persuasive LLM debate follow-up | Medium — verify |

Also verify: A2A protocol status (Linux Foundation transfer date, current adopters), MCP version specifics, AgentCore feature set as of May 2026 (Microsoft Learn was the only live source confirmed during research).

---

## Recommended next steps (after Gary's review)

If we get directional agreement on this brief, the proposed order — **specialized for the BA vertical, prioritized by impact and dependency** — is:

1. **Baseline against a BA-specific eval corpus.** External anchor before architectural changes. TheAgentCompany (A12) is the generic anchor; we also need a BA-specific one — start by curating a small corpus of historical BA deliverables we can score against.
2. **Documentation-surface connector layer (BA6).** Without this, the BA agent has no data; without data, none of the rest matters. Start with Confluence + Notion + Linear + Drive; add others by demand.
3. **Requirements traceability matrix (BA3) + two-mode discovery (BA2).** The data structures the BA work runs on. Also unblocks BA7.
4. **Citation-locked decision artifacts (BA1) + capability routing (A2).** Citation discipline is the cheapest dignity-of-output feature; routing is the foundation for everything downstream.
5. **Draft-then-defend (BA-specialization of A3, separate-model-family auditor) + adversarial deck critique (BA5).** The two quality gates that catch the worst BA failure modes.
6. **Stakeholder interrogation protocol (BA4) + spec-challenge (A13).** Interrogated discovery as a structured workflow.
7. **Reversibility-graded tripwires (A6) + retirement of novelty-primary (R3).** General safety.
8. **Magentic-One dual-ledger in Legatus AI (A1).** Coordination substrate; required before fleet scale exceeds ~5 concurrent agents.
9. **Outcome attribution (BA7).** Closing the loop from artifact to business metric. Needs BA3 in place first.
10. **Honeypot canaries (A7), runtime interpretability probes (A8).** General safety stack.

Items not enumerated (A4, A5, A9, A10, A11, A12, A14; capability tokens migration) hold their positions in the general adoption list and slot in as capacity allows. The quarantine (R5) holds until external red-team signs off; no time gate.

---

## Closing note

The honest framing for Gary: Sentinel as it stands is the most rigorous proof-of-work hook engine for agentic sessions that exists in the wild. The recommendations above don't replace it — they extend it from a unit-of-work enforcement engine toward a future fleet-scale coordination plane with safety primitives the literature actually underwrites. The retirements aren't criticisms of what's built; they're things we shouldn't *add* (or should remove if creeping in) because the field has learned, in the last 18 months, that they don't work or actively cause harm.

The single most important sentence in the brief: **the architecture today is excellent scaffolding against an agent trying to help, and almost no defense against an agent trying to look like it is helping.** The adoption list closes that gap as honestly as the current literature allows; the retirement list keeps us from widening it.
