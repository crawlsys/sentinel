# Competitive Intelligence — Federated Agent Gateways with Cryptographic Execution

**Owner:** Gary Somerhalder
**Cadence:** Quarterly. Next review: **end Q3 2026** (target: 2026-09-30).
**Origin:** Task #80. See [CONTRIBUTING.md](../CONTRIBUTING.md) for sentinel's own positioning principles.

## Why this exists

The category sentinel sits in — **federated agent gateways with cryptographic execution proofs** — formed during 2025-2026 and is still in motion. Apollo, IBM, Microsoft, EQTY Lab, and the AEGIS policy library each ship overlapping primitives; whichever combination wins on enterprise adoption will set the de facto standard. Sentinel's positioning depends on knowing what's shipped in the others *and* what's announced for the next quarter.

The doc is a snapshot, not a long-running log. Each quarterly review replaces the *Current state* block. Prior reviews go into the *History* section.

## What we're tracking

Five named players plus an "other / new entrants" bucket. For each, we record:

- **Latest release** — version + date.
- **Architecture posture** — gateway-only, runtime-included, both?
- **Cryptographic story** — Merkle/hash-chain, signing, post-quantum?
- **Policy/governance story** — declarative, AI-suggested, hard gates?
- **Multi-vendor story** — single-vendor lock-in or pluggable?
- **Threat to sentinel** — none / wide-moat / direct overlap / leapfrog.
- **Action** — none / file Linear ticket / file design note.

## Current state (snapshot: 2026-05-14, ahead of first formal Q3 review)

### Apollo (MCP Server, GraphQL Federation lineage)

- **Latest:** Apollo MCP Server (Q4 2025 GA), continued shipping through 2026.
- **Architecture:** Gateway-as-product. Composes subgraphs into a federated supergraph. Has the **strongest** subgraph composition story in the category — `apollo composition check`, contract variants, persisted queries, query plan caching are all real.
- **Cryptography:** None native. Treats trust as RBAC + signed manifests.
- **Policy:** Schema-first. `@key`, `@requires`, `@provides`, `@external`, `@composeDirective`, `@deprecated`, `@override` directives. Sentinel's federation_cmd directly mirrors this surface (commits 69de559, 249a1b9, 2fe740e).
- **Multi-vendor:** Vendor-agnostic by design — Apollo's whole pitch is "compose anything." LLM-agnostic in principle.
- **Threat to sentinel:** **Wide-moat** (no direct overlap on cryptographic execution proofs). The federation primitives are the most-developed in the category and we *deliberately* mirror them rather than compete. Sentinel's value-add over Apollo is the StepProof chain + AI judge + Doppler personal-branch identity — all things Apollo doesn't ship.
- **Action:** None this quarter. **Watch for:** native cryptographic trust primitives appearing in Apollo Federation v3 or a successor — if they ship Merkle chains over federated subgraphs, the moat narrows.

### IBM ContextForge

- **Latest:** ContextForge releases through Q1 2026 (review fresh release notes next quarter — version drift moves fast here).
- **Architecture:** Gateway + runtime — enterprise MCP gateway with built-in observability and rate limits.
- **Cryptography:** SSRF guard, content size caps, rate limits — all enterprise hardening. **No execution proof chain.**
- **Policy:** Pydantic-style fail-fast config validation (sentinel mirrored this in M4.8 / commit 85e0511). Default-block private network ranges (mirrored in M4.7 / commit 208cc13).
- **Multi-vendor:** IBM-aligned but framework-agnostic. Targets enterprise MCP deployments rather than developer workflows.
- **Threat to sentinel:** **Wide-moat** on cryptographic execution. **Direct overlap** on operational hardening — they ship more enterprise polish than we do today (#23/#24/#25 from earlier roadmap). Sentinel already absorbed those patterns; we're not behind on the substantive parts.
- **Action:** None. **Watch for:** ContextForge adding signed evidence or chain-of-custody primitives — they have the enterprise customer base to drive that demand faster than we can.

### Microsoft Agent Governance Toolkit (AGT)

- **Latest:** AGT cadence is Microsoft-Build-aligned (May/November). Check 2026-05-Build announcements (just happened ~today; review at end of Q3 once dust settles).
- **Architecture:** Governance overlay, not a gateway. Plugs into existing agent platforms (Azure AI Foundry, Copilot Studio) rather than running the agents itself.
- **Cryptography:** Ed25519-signed plugin manifests — sentinel mirrors this design (#26 / M2.13 deferred). Aggregate trust scoring per agent (0-1000, five tiers) — sentinel shipped this in commit d6ce61b (M6.3).
- **Policy:** Microsoft-owned schemas, declarative governance, runs alongside but doesn't dictate the agent runtime. Agnostic on which runtime (the framework-agnostic story IBM and Apollo both push).
- **Multi-vendor:** Microsoft-flavored but explicitly framework-agnostic. Aimed at compliance officers, not engineers.
- **Threat to sentinel:** **Direct overlap** on trust scoring + signed manifests. **Wide-moat** on per-developer Doppler personal-branch identity (no equivalent in AGT — they're org-level, not developer-level). Microsoft's distribution to enterprise IT is the real leverage; if a buyer asks "what does Microsoft offer," AGT is the answer we lose to.
- **Action:** **Watch closely** — file Linear ticket if AGT ships a developer-personal-identity primitive in 2026-11-Build. That would directly compete with our per-developer Doppler personal-branch pattern (#90-#100).

### EQTY Lab

- **Latest:** EQTY Lab agent governance product, ongoing releases.
- **Architecture:** Governance-as-product, similar to AGT but smaller / more focused. Strong on **process isolation** as integrity boundary (sentinel mirrors this in #60 / M7.11 — step_judge in separate sandboxed process, deferred).
- **Cryptography:** Cryptographic proofs of agent execution. **Closest direct overlap** with sentinel's StepProof chain — they have the same instinct that hash-chained evidence + AI judge is the right primitive.
- **Policy:** Behavioral anomaly detection (sentinel mirrors in commit 409d721 / step_anomaly hook, M1.9). 9-dimensional anomaly axes from the AEGIS pattern.
- **Multi-vendor:** Vendor-agnostic by design.
- **Threat to sentinel:** **Direct overlap** on the proof chain — the closest thing to a sentinel competitor in the named players. Differentiation today: sentinel ships the per-developer identity story (Doppler) and the schema-first federation surface (Apollo lineage), neither of which EQTY has prominently.
- **Action:** **File Linear ticket** at next Q3 review if EQTY ships any of: (a) Doppler/personal-secret integration, (b) Vulcan-like SDK that other MCP authors adopt, (c) federation/composition primitives. Any of those would narrow the moat materially.

### AEGIS policy library

- **Latest:** Open-source policy library, continuous.
- **Architecture:** Library, not a runtime. Provides policy primitives (cold-start baselines, 200-trace anomaly thresholds, plain-English-to-config translation, kill-switch patterns).
- **Cryptography:** No native cryptography — relies on the runtime that adopts it.
- **Policy:** **The reference** for the "policy as code" pattern that sentinel mirrored in M1.7/M1.8/M1.9 (Ed25519 + cold-start baseline + 9-dim anomaly).
- **Multi-vendor:** Library-shaped, runtime-agnostic by definition.
- **Threat to sentinel:** **None** — it's a library, sentinel consumes its ideas, AEGIS doesn't compete on runtime. If anything, broader AEGIS adoption helps sentinel (more consumers means the patterns we already ship become more legible to buyers).
- **Action:** None. Cite AEGIS in marketing material when describing our policy primitives — borrowed-credibility win.

### Other / new entrants

- LangGraph (LangChain ecosystem, durable execution).
- Temporal AI agents (Temporal SDK, durable workflow story).
- Convex agents (real-time + agent runtime).
- Various open-source MCP gateways announced through 2026.

**Action:** Survey at Q3 review. If any have shipped proof-chain primitives + per-developer identity + federation composition, they jump to the named list.

## Sentinel positioning (as of 2026-05-14)

Where sentinel lives in this map:

| Primitive                                | Sentinel ships? | Closest competitor that ships it          |
|------------------------------------------|-----------------|-------------------------------------------|
| Hash-chained step proofs                 | ✅ M1.1-M1.5    | EQTY Lab                                  |
| Ed25519 signing (opt-in)                 | ✅ M1.7         | Microsoft AGT (mandatory)                 |
| 9-dim behavioral anomaly detection       | ✅ M1.9         | EQTY Lab + AEGIS                          |
| Cold-start baseline                      | ✅ M1.8         | AEGIS                                     |
| AI judge with multi-model verification   | ✅ #69 (Stage A+B) | EQTY Lab                                |
| Schema-first federation composition      | ✅ M2.4/M2.5/M2.6/M2.7/M2.8/M2.9 | Apollo (deeper)             |
| Per-developer Doppler personal-branch    | ✅ #90-#100    | **None named** (sentinel novelty)         |
| Aggregate trust score (0-1000, 5 tiers)  | ✅ M6.3         | Microsoft AGT                             |
| Cross-session proof archive              | ✅ #73          | None named                                |
| SSRF + rate limits + size limits         | ✅ M4.6/M4.7    | IBM ContextForge                          |
| Pydantic-style fail-fast config          | ✅ M4.8         | IBM ContextForge                          |
| Per-step circuit breakers + retries      | ✅ M4.4         | Apollo (analogous for queries)            |
| Apollo `@deprecated`/`@override`         | ✅ M2.6         | Apollo                                    |
| Plugin extensibility (`extra` field)     | ✅ M2.9         | Apollo                                    |
| Signed plugin manifests                  | ❌ M2.13 deferred | Microsoft AGT                           |
| Hardware-rooted signing (HSM)            | 🗑 scoped out (see memory) | Microsoft AGT (FIPS path)        |
| Post-quantum signatures (ML-DSA)         | 🗑 scoped out  | None named ship it yet                    |
| Streamable HTTP transport                | ❌ M2.12 deferred | Apollo                                  |
| Plain-English policy → config            | ❌ M7.10 deferred | AEGIS                                   |
| Persisted/frozen plans for prod          | ❌ M7.7 deferred | Apollo (persisted queries)                |
| Temporal durability                      | ❌ M8 deferred  | Temporal (purpose-built)                  |

**The novel-to-sentinel column** is the answer to "why us over them": per-developer Doppler personal-branch identity. The rest is sentinel + Apollo + Microsoft AGT + IBM ContextForge + EQTY Lab + AEGIS all converging on the same primitive set, faster than buyers can tell us apart on Twitter.

## Review process (use this template each quarter)

1. Re-read each named player's release notes since the last snapshot date.
2. Update *Current state* in place (don't preserve old text — push it to *History* below).
3. For each player, fill the seven fields (latest / architecture / crypto / policy / multi-vendor / threat / action).
4. Update the sentinel positioning table — mark new ships, mark new competitors.
5. File Linear ticket for any "action: file Linear ticket" rows.
6. Update *Next review* date at the top.

## History

### 2026-05-14 (initial framework, ahead of first formal Q3 review)

First-pass snapshot. No quarterly review formally completed yet — this section will fill as Q3 / Q4 / 2027-Q1 reviews land. Each will start with a one-paragraph executive summary: what shipped that mattered, what threats appeared, what action was taken.
