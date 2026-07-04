# BA6 — Connector Layer Scoping

**Status:** Proposed (scoping doc; pending operator ratification of direction, not detail)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **BA6** (S-tier; product-critical)
**Related:**
- `docs/policy-replay-mining-quarantine.md` (R5)
- `docs/policy-no-auto-summary-without-critique.md` (R8 / acute in BA context)
- Legatus AI side ADRs at `legatus-ai/legatus-ai:docs/adr/` — ADR-016 (Legatus AI Peer Registration), ADR-017 (Artifact + Requirement Metadata Extensions), forward-reference ADR-018 (Legatus AI Peer Capability Tokens, not yet drafted).

---

## TL;DR

BA6 is the connector layer that feeds the BA-vertical product its data. The brief calls it S-tier and product-critical: without data, no BA product. This doc scopes — not designs — the connector layer:

- **What connectors first** (Confluence, Notion, Linear, Drive — staged by usage), **why those**, **what shape they take** (MCP servers consumed by the BA-orchestrator, *not* sentinel-internal hooks).
- **Ownership boundary**: connectors are external MCP servers. Sentinel observes their use (via existing `mcp_health` and audit hooks); Legatus AI carries the provenance metadata they produce. **Sentinel does not host connectors.**
- **Contract every connector must honor**: provenance metadata on every artifact, content-hashing for change detection, scoped capability tokens, audit emission, failure-mode taxonomy.
- **What is *not* in scope** for this doc: any actual implementation, vendor SDK choice, deployment topology, or commercial connector ecosystem decisions.

This is a *direction* document. The operator ratifies direction; the connector contract becomes a separate ADR once implementation begins.

---

## 1. Why BA6 is S-tier and why it doesn't live in sentinel

### Product-critical

BA1 (citation-locked decision artifacts) demands that every claim in BA output have a source pin. BA2 (two-mode discovery) splits data acquisition into *automated* (system pulls) and *interrogated* (stakeholder Q&A). BA3 (requirements traceability matrix) wants every recommendation traced back to a stated need. All three are downstream of having actual data to cite, trace, and reason about. Without the connector layer, the BA-orchestrator is operating on whatever the operator pasted in — i.e., a writing assistant, not a Business Analyst.

### Ownership boundary

Connectors are **external MCP servers** consumed by the AI-factory's BA-orchestrator. They are not sentinel hooks. They are not Legatus AI protocol additions. They live as separate processes that:

- Expose MCP tools (`mcp__confluence__list_pages`, `mcp__notion__read_page`, `mcp__linear__list_issues`, `mcp__drive__search`, etc.).
- Authenticate to upstream systems with operator-issued credentials.
- Carry provenance metadata on every artifact they return (per BA1).
- Hash artifact content at retrieval time for change detection (per BA1).
- Emit audit hooks consumable by sentinel (per Constitution Rule 10 in Legatus AI, mirrored by sentinel's audit-by-construction posture).

Sentinel's job is to *observe* connector use (existing `mcp_health` hook) and to *gate* mutating connector calls if applicable (existing `tool_usage_gate` extension). Sentinel does **not** ship the connectors.

Legatus AI's job is to *carry* connector-produced metadata through the `RelayInstruction` and `InstructionResult` messages (per Legatus AI ADR-017). Legatus AI does **not** ship the connectors either.

The connectors are their own component class. This scoping doc lives in sentinel because the BA-vertical product strategy lives in sentinel; the connector contract is *consumed* by sentinel-observed flows.

---

## 2. Which connectors first

The brief names Confluence, Notion, Linear, Drive as the starter set. Rationale, prioritized:

### Tier 1 — start here (in this order)

| Connector | Why first | Brief BA primitives served |
|---|---|---|
| **Linear** | Already wired as MCP in this environment (`linear-mcp` is the one MCP currently registered in `~/.claude.json`). Provides ticket/issue corpus that BA3 (traceability matrix) maps directly onto. The traceability matrix lives in Linear-shaped data. Zero new infrastructure to start exercising BA3. | BA3 (matrix), BA1 (citation source), BA2 (automated discovery) |
| **Confluence** | Standard BA documentation system of record. Page-and-paragraph-anchor citation maps cleanly to BA1's `source_uri` + `paragraph_anchor` requirement. Largest single source of unstructured business requirement text in most orgs. | BA1, BA2, BA3 |
| **Notion** | Adjacent to Confluence in role (org docs, requirements, project briefs). Block-id anchors satisfy BA1 citation requirements. Many newer orgs use Notion in place of Confluence — same role, similar contract. | BA1, BA2, BA3 |
| **Google Drive** | Catch-all for unstructured documents (PDFs, slides, sheets). Lowest fidelity for citation (no native paragraph anchors); content_hash + page-number anchor is the floor. | BA1, BA2 |

### Tier 2 — by demand

The brief mentions Jira, SharePoint, Slack, Gmail, M365, Atlassian. These add value but each is a real engineering investment. Recommend gating each on a concrete operator request rather than building speculatively. Slack and Gmail in particular have substantial provenance complexity (threads, edits, attachments) that deserves real design time once usage is known.

### Anti-pattern to avoid

Don't build a "generic connector framework" first. Build Linear → Confluence → Notion → Drive as concrete connectors, see the patterns repeat, then extract the framework. The brief's R1 retirement (no role personas as primitives) generalizes: don't build abstractions ahead of two concrete instances.

---

## 3. The contract every connector must honor

Each connector is an MCP server exposing a small set of tools. The shape of those tools is connector-specific, but every connector must conform to this contract:

### 3.1 Provenance metadata on every returned artifact

Every artifact a connector returns carries (matches `ArtifactReference` in Legatus AI ADR-017):

- `artifact_id` — opaque, connector-namespaced (e.g., `confluence://space/page-id`).
- `artifact_type` — Document / Spreadsheet / Slide / EmailThread / ChatThread / CodeFile / Database / Other.
- `source_uri` — canonical URI the human (or another agent) can paste into a browser to reach the source.
- `content_hash` — versioned hash (SHA-256 or Blake3 per Legatus AI ADR-017's `ContentHash`). Computed over the canonical text content at retrieval time. Enables change detection downstream.
- `retrieved_at` — UTC timestamp of the retrieval.
- `provenance_class` — SystemOfRecord / Interview / Inference / ExternalApi. (Most connector returns are SystemOfRecord; the enum exists so the BA-orchestrator can mix in interview-sourced or inferred artifacts and audit them appropriately.)

### 3.2 Content-hashing discipline

Every artifact returned is hashed at retrieval time. The hash is used downstream for:

- **Change detection** — if the same `source_uri` is re-pulled and the hash differs, the BA-orchestrator knows the source has changed since prior citation.
- **Audit verifiability** — when the BA's recommendation is reviewed later, the audit log records the hash; if the source has since changed, the change is detectable rather than silent.
- **BA1 enforcement** — sentinel's audit hook can refuse to record a citation that lacks a content_hash.

### 3.3 Scoped capability tokens

Connectors authenticate to upstream systems with credentials issued by the operator. The connector itself authenticates *its consumers* (BA-orchestrator, human, whatever) using capability tokens. The token scope determines what the consumer is permitted to read.

Concrete contract:
- Connector exposes a `whoami`-style tool that returns the consumer's effective capability set without making an upstream call.
- Read scopes are granular (per-space, per-database, per-folder) where the upstream system supports it.
- Write scopes are *separately* opt-in and default-off (most BA work is read-only).
- Token revocation is a connector-side concern (the BA-orchestrator's token is revocable independently of the underlying upstream API key).

This contract aligns with Legatus AI's deferred **ADR-018 — Legatus AI peer Capability Tokens** (PASETO v4.local). When ADR-018 lands, the same token primitive is used for both Legatus AI peer auth and connector auth. Until then, connectors run in sandbox mode (operator-issued, no fine-grained scoping).

### 3.4 Audit emission

Every connector call emits a structured audit event consumable by sentinel and Legatus AI. Minimum required fields:

- `connector_name` (e.g., `confluence`)
- `tool_name` (e.g., `list_pages`)
- `consumer_identity_label` (whoever called — BA-orchestrator instance, human, etc.)
- `target_resource_summary` (no full content; just enough to identify what was accessed)
- `outcome` (Success / Failure / Denied)
- `latency_ms`
- `bytes_returned`

Sentinel's existing `mcp_health` hook is the natural sink. Legatus AI's `AuditEvent::ExternalApiCall` (already in the Legatus AI taxonomy) is the natural transport when the call traverses Legatus AI.

### 3.5 Failure-mode taxonomy

Every connector must distinguish and surface, with no error-collapsing:

- **Upstream unavailable** — connector reached upstream API but got 5xx / network failure.
- **Upstream authentication failed** — token rejected by upstream (often operator credential expiry).
- **Capability denied** — connector's own capability check rejected the consumer.
- **Resource not found** — upstream returned 404 / equivalent.
- **Rate limited** — upstream returned 429 / equivalent; connector returns retry-after.
- **Content too large** — connector refuses payloads beyond the 1 MiB envelope limit (Legatus AI ADR-013); BA-orchestrator must page or summarize.
- **Content redacted** — connector applied a configured redaction rule (PII, secrets); returns metadata-only.

Each is a distinct error class with stable code. The BA-orchestrator's decision tree depends on which class — it should retry rate-limited but escalate auth-failed.

---

## 4. Hex / DDD integration with sentinel's MCP surface

Sentinel today registers MCP servers in `~/.claude.json`. Each connector is one entry there:

```jsonc
{
  "mcpServers": {
    "linear": { "command": "/Users/.../linear-mcp", "type": "stdio" },
    "confluence": { "command": "npx", "args": ["-y", "@org/confluence-mcp"], "type": "stdio" },
    "notion": { ... },
    "drive": { ... }
  }
}
```

Sentinel's existing hooks interact with each in well-defined ways:

| Hook | Connector interaction |
|---|---|
| `mcp_health` (existing) | Periodically pings each connector; reports availability + latency to sentinel metrics. |
| `tool_usage_gate` (extended per the hook-quality ADR) | Gates *mutating* connector calls (e.g., a `notion__create_page` from BA-orchestrator) with sentinel's standard four-check stack. Read-only calls are exempt. |
| `audit_extract` (new — proposed) | Lifts the connector's audit-event emission into sentinel's audit chain. One line per call into the sentinel audit log. |
| `provenance_validate` (new — proposed) | On any BA-orchestrator output (artifact citation, recommendation), validate that every cited `artifact_id` has a corresponding audit record from a connector call. Catches BA1 violations structurally. |

**Two of these are new hooks.** `audit_extract` and `provenance_validate` are not in the current 27-hook set; they belong in `crates/sentinel-application/src/hooks/` if/when implementation begins. Out of scope for this scoping doc; flagged for the follow-up implementation ADR.

### Hex/DDD impact

- Connectors are external processes. They don't live in `sentinel-domain`, `sentinel-application`, or any sentinel crate.
- Sentinel's interaction with connectors is mediated by `tool_usage_gate` and the new audit hooks. These all live in `sentinel-application/src/hooks/`.
- Connector contracts (the data shapes returned) are mirrored in Legatus AI's `ArtifactReference` (already defined in Legatus AI ADR-017). Sentinel and Legatus AI agree on the wire shape; the connector's job is to produce it.
- No new ports in `sentinel-domain`. No new ports in `legatus-ai-domain`. The connector layer is *adjacent* to both, consumed by the BA-orchestrator (which is a Legatus AI class peer per Legatus AI ADR-016).

---

## 5. Capability-token plumbing

Phase 1 (today through ADR-018 ratification):
- Connectors accept an operator-issued opaque token at startup (env var, e.g., `CONFLUENCE_TOKEN=...`).
- Consumer-side identity is `SandboxConsumer { label }` — same shape as Legatus AI ADR-016's `DispatcherIdentity::Sandbox`. Non-prod only.
- Capability scoping is whatever the upstream API supports natively (per-space, per-page, per-token).

Phase 2 (after Legatus AI ADR-018 ratifies):
- Connectors verify a PASETO v4.local capability token presented by the consumer.
- The capability set in the token determines what the connector permits (read:confluence:space-X, write:notion:database-Y).
- Token revocation flows through Legatus AI's planned Legatus AI peer revocation mechanism.

This phased approach avoids blocking BA6 on ADR-018. Phase 1 unblocks the BA-vertical product in a sandbox; Phase 2 hardens it for any production deployment.

---

## 6. Failure modes specifically anticipated

### 6.1 Connector outage during BA workflow

If a connector goes down mid-orchestration, the BA-orchestrator must:
- Surface the outage as a `SessionBlocked` event to Legatus AI (per Legatus AI protocol; visible to human operators in their voice/text legatus-ai-app).
- Not silently fall back to inference. R8 (no auto-summary without critique) applies recursively here — inferred-because-connector-was-down content reaching an exec deck is the catastrophic case.
- Preserve the partial state so the orchestration can resume when the connector returns.

### 6.2 Stale citations

A BA recommendation may cite an artifact that has since changed at the source. Content-hashing makes this detectable, but the system must do something with the detection:
- On every citation render (deck slide, brief paragraph, memo line), the BA-orchestrator should re-validate the cited artifact's hash within a configurable freshness window (e.g., 24h).
- Stale citations are flagged in the artifact for human review. They are *not* silently re-pulled — silent re-pull would invalidate the trail.

### 6.3 Rate limiting under load

A multi-orchestration AI factory could hit upstream rate limits aggressively. Connectors must:
- Honor 429 with backoff.
- Surface sustained rate limiting as a connector-health degradation (visible via `mcp_health`).
- Coordinate across multiple orchestrations sharing the same upstream credentials (token bucket per upstream-account, not per-orchestration).

### 6.4 Cross-tenant leakage

If multiple BA-orchestrators connect to the same connector instance (e.g., one Confluence connector serving multiple BA workflows for different customers), the capability-token scoping must prevent one orchestration from seeing another's content. This is exactly what ADR-018 fine-grained capability sets exist for. Phase 1 (sandbox) is single-tenant only; Phase 2 (PASETO) enables multi-tenant.

### 6.5 Connector compromise

If a connector binary is itself compromised (supply-chain attack on a vendor MCP), the operator-issued upstream credentials are exposed to whatever the attacker controls. Mitigations:
- Connectors run with the minimum upstream scope they need (read-only by default; write opt-in).
- Operator monitors connector binary integrity (hash check on update; pin to versions; this is general supply-chain hygiene).
- Sentinel's `tool_usage_gate` extends to MCP calls — a compromised connector calling unexpected upstream APIs trips the gate (assuming the gate's tool-classification is up to date).

---

## 7. What is explicitly *not* in scope for this doc

- **Actual connector implementations.** No code; no vendor SDK choice; no deployment topology.
- **Specific MCP server frameworks.** Whether to use Vulcan, Anthropic's MCP SDK, ts-mcp, etc. is a per-connector implementation choice.
- **Commercial connector ecosystem decisions.** Whether to build connectors in-house, fork open-source ones, or pay vendors is a business decision orthogonal to the contract.
- **The BA-orchestrator's internal use of connector output.** That belongs in a separate BA-orchestrator design doc, not this scoping doc.
- **The two new sentinel hooks** (`audit_extract`, `provenance_validate`). Flagged for a follow-up implementation ADR.
- **Legatus AI ADR-018 details.** Forward-referenced but not designed here.

---

## 8. Why this is a "scoping" doc, not an ADR

ADRs commit to specific decisions ("we chose CBOR over JSON because X"). This document instead establishes:

- **Direction** (start with Linear/Confluence/Notion/Drive; not generic framework first).
- **Boundaries** (connectors live outside sentinel; sentinel observes and audits; Legatus AI carries metadata).
- **Contract** (provenance, content-hash, capability tokens, audit, failure modes).
- **Ownership** (operator owns upstream credentials; orchestrator owns capability tokens; sentinel owns observation; Legatus AI owns transport).

Each *specific* decision (which connector first ships, which framework it uses, what exact tool surface it exposes) becomes its own implementation ADR when work starts. This doc exists so those implementation ADRs don't reinvent the contract.

---

## 9. Decision and ownership

- **Decision class:** scoping / direction. Not a quarantine (R5), not a governance retirement (R1/R2/R3/R8/R14), not an architectural commitment (an ADR).
- **Owner:** The operator ratifies direction; per-connector implementation ADRs need separate ratification.
- **Re-evaluation cadence:** revisit when the first connector ships, to see if the contract held up under real implementation.
- **Related items in the brief:** BA6 (this scoping), BA1 (citations — direct downstream), BA2 (two-mode discovery — direct downstream), BA3 (traceability matrix — direct downstream).

---

## 10. Methodology

No literature citations needed — this doc is a direction-setting scoping for an integration layer that's already conventional in the BA-tooling market. The connector pattern is well-trodden (every BA tool from Tableau to Looker to ThoughtSpot to Hex maintains its own connector layer). The novelty here is the *contract* (provenance + content-hash + capability-token + audit) imposed on top of the conventional pattern, driven by the BA-vertical product's trust requirements per R8 and BA5.

## 11. Ratification

This document is **proposed direction**. Operator ratification commits Sentinel to:
- The starter connector set (Linear, Confluence, Notion, Drive in that order).
- The contract every connector honors (§3).
- The hex/DDD-respecting integration shape (§4).
- The phased capability-token plumbing (§5).

Per-connector implementation ADRs follow, each separately ratifiable.

**Ratified by:** _________________________
**Date:** _________________________
