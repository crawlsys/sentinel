# Contributing to Sentinel

This document is the architectural contract for the federated step
proof system. It exists because every design decision we make has
downstream consequences that are expensive to undo, and we'd rather
write the rules down once than re-litigate them per PR.

If you're touching the proof chain, the federated step namespace
(`config/steps/`), the skill router, or anything that crosses the
sentinel-domain / sentinel-application / sentinel-infrastructure
boundary, **read this first**.

## The Bible

> **For every action the agent claims to have taken, there must
> exist evidence captured from a system the agent does not control.
> Absent such evidence, the claim is rejected by the judge.**

This is the load-bearing rule. Every layer of the system serves it:

- **Hash chains (M1.1, M1.2)** make tampering with past claims
  computationally infeasible.
- **Step gates (M1.3)** prevent skipping ahead — you can't claim work
  on step N+1 without a proof for step N.
- **AI judges (M1.4)** evaluate whether the work actually happened
  based on the captured evidence.
- **External evidence adapters (#77, the bible task)** ensure the
  evidence comes from systems the agent didn't author — GitHub API,
  Linear, Doppler, the filesystem, Browserbase recordings.
- **Cross-vendor judging (#73)** prevents a single LLM family's drift
  from corrupting verdicts.
- **Ed25519 signing (M1.7)** proves *sentinel itself* wrote the chain.

When you're adding a feature, ask: **does this strengthen "every
claim has a receipt" or weaken it?** If it weakens it, push back on
the requirement before you ship the code.

## The Apollo Federation principles

Sentinel's federated step namespace is structurally Apollo Federation
v2 applied to AI agent execution. Apollo's hard-won design lessons
apply directly. Adopt them; don't rediscover them.

### 1. Schema-first, code-second

The TOML in `config/steps/<skill>.toml` is the source of truth, not
the Rust code. When step definitions and Rust code diverge, the TOML
wins. Code is generated *from* schemas (M2.2's build.rs codegen),
never the other way around.

**Why**: schemas describe contracts, code describes implementations.
Federation requires every subgraph (skill) and the gateway (router)
to agree on contracts; if you let code drift independently, the
federation breaks at composition time.

**In practice**:
- Adding a step: edit `config/steps/<skill>.toml` first, then
  build skills-mcp to regenerate the `#[tool]` functions.
- Changing a step's signature: bump `federation_version` in the
  TOML, run `sentinel federation check`, fix what the report calls
  out before merging.
- Never hand-edit generated code in skills-mcp's OUT_DIR.

### 2. Demand-driven design

Don't add steps speculatively. The set of steps grows from observed
agent behavior — what skills actually invoke, what tool combinations
the router emits, what verdicts the judges produce. The proof corpus
is the demand signal; mine it before adding new step types.

**Why**: every step you add is forever — once a chain references it,
removing it is a deprecation cycle. Apollo learned this the hard way
with v1→v2 migration. We get to skip the lesson.

**In practice**:
- Before adding step type `X` to a skill, run
  `sentinel federation compose` and check whether existing steps
  cover the use case.
- Use `query_proof_corpus` (M4.3) to look at what step sequences
  agents have actually executed. If the data shows agents
  combining existing steps to do X, don't add X — improve the
  combination.

### 3. Skill subgraphs own their data

A skill's step config is the only authority on that skill's steps.
Other skills, the router, the dashboard — none of them reach into
another skill's proofs/state directly. They go through typed
StepProof artifact handoffs.

**Why**: this is the federation property. Apollo subgraphs that read
each other's databases break federation; sentinel skills that read
each other's StepProofs break it the same way. The chain of types
is the chain of trust.

**In practice**:
- Skill A's step needs data from skill B's prior step? It consumes
  the StepProof artifact from skill B as typed input. It does NOT
  read skill B's chain directly.
- Tests that need cross-skill data should construct the StepProof
  by calling `submit_step_evidence`, not by reaching into another
  module's internal state.

### 4. Gateway plans, doesn't execute

The skill router's job is to emit a virtual skill pack — a DAG of
which steps to call from which skills, with typed handoffs. It does
NOT execute the steps. Execution lives in skills-mcp / agents-mcp.

**Why**: Apollo enforced this separation because routers that also
ran resolvers became unmaintainable monoliths. The federation
gateway plans queries; subgraphs answer them. We mirror that
exactly.

**In practice**:
- skill_router (sentinel-application) emits a VirtualSkillPack and
  hands it to skills-mcp's WorkflowRuntime to execute.
- The router never directly invokes step bodies. If you find
  yourself wanting to "just do this small thing in the router,"
  you're about to break the boundary. Stop.

### 5. Implementation details stay in subgraphs

Federation handoffs are typed StepProof artifacts. They are NOT
markdown blobs, raw config file paths, internal state dumps, or
free-form JSON. Every cross-skill handoff goes through a defined
artifact schema; if no schema exists yet, define it before adding
the handoff.

**Why**: typed contracts are the difference between composition and
"two skills awkwardly bolted together with regex." Apollo learned
this; we're applying it.

**In practice**:
- Cross-skill data in StepProof.artifact is always structured JSON
  matching a schema documented in the producing step's config.
- Consumers parse the typed structure, not free-form text.
- If a downstream step would have to regex-extract a URL from a
  description string, the producing step's artifact schema is
  wrong — fix it.

## The trust layers (in order)

Add features in the layer where they belong. Each layer assumes the
ones below it are correct.

| Layer | Owns | Validates |
|------|------|-----------|
| 1. Cryptography | SHA-256 chain, Ed25519 signing | Internal hash + signature integrity |
| 2. Sentinel writes the chain | Hooks, ProofEngine | Agent doesn't construct hashes |
| 3. AI judge | step_judge, JudgeService | Did the work pass evaluation |
| 4. External evidence | Adapters (#77) | Receipt exists from a non-agent system |
| 5. Cross-vendor diversity | OpenRouter judge pool (#73) | No single-vendor model drift can corrupt verdicts |
| 6. Behavioral anomaly | step_anomaly (M1.9) | Run looks normal vs historical baseline |

If your feature claims to add a property that crosses layers, you're
probably designing wrong. Properties layer; they don't span.

## Testing discipline

### Every commit ships green tests

No "I'll add tests in a follow-up." The test count is monotonic.
Every commit adds tests for the code it adds; the workspace passes
`cargo test --workspace` before every push.

The current invariant: **sentinel-domain + sentinel-application +
sentinel-engine = X passing, 0 failing**. If you don't know what X
is, you haven't run the tests recently enough.

### Test the contract, not the implementation

Bad: "this function returns Vec with 3 elements when called with X"
Good: "this function rejects insufficient verdicts" — exercises
behavior at the level callers actually depend on.

If a refactor changes implementation but not behavior, the tests
should still pass without modification. If they don't, the tests
were testing implementation.

### Tests are documentation

Test names are read more often than function names. Write them as
sentences:
- ✅ `test_step_3_blocked_until_step_2_proof_exists`
- ❌ `test_step_gate_works`

When someone touches your code in 18 months, the test names tell
them what the code is supposed to do. Optimize for that reader.

## Commit discipline

### One logical change per commit

A "feat(judge): cold-start baseline" commit changes the judge code,
the state to track baselines, the tests for the new behavior. It
does NOT also fix a typo in an unrelated file. The typo gets its
own commit.

Why: when this code breaks in 6 months, `git bisect` lands on the
commit that broke it. Bisecting through a 30-file commit is useless;
bisecting through a focused one points directly at the bug.

### Commit messages explain WHY, not WHAT

The diff shows what changed. The message must explain why the
change exists and what alternative was considered.

Bad: "Add baseline counter to SessionState"
Good: "feat(judge): cold-start baseline — defer enforcement until
N successful judgements. AEGIS pattern. Prevents new skills being
unusable day-one due to over-strict initial AI judgements. Per-
session today; cross-session persistence is filed as #78."

The good version tells future-you (and future contributors) what
problem this solves and what tradeoffs were considered. The bad
version says nothing the diff doesn't already.

### Reference task numbers and design docs

If a commit relates to a milestone (M1.7), task ID (#73), or
design doc, name them. Search will find them years from now.

## Adding new components

### Adding a hook

1. Create `crates/sentinel-application/src/hooks/<name>.rs`.
2. Register in `hooks/mod.rs`: `pub mod <name>;` AND add to
   `HOOK_NAMES`.
3. Wire into `hook_cmd.rs` for the appropriate event (PreToolUse,
   PostToolUse, etc).
4. Tests inline in `mod tests`. Cover: happy path, glass-break
   bypass, the obvious adversarial path, the boring "tool name
   doesn't match" no-op.
5. Use `[Sentinel-Authority]` prefix on any deny message — that's
   the contract for Claude Code's runtime to hard-reject.

### Adding a step config

1. Edit `config/steps/<skill>.toml`.
2. Run `sentinel federation compose` — must report 0 errors.
3. Bump `federation_version` if the change is breaking (M2.7).
4. Add a step config test under `crates/sentinel-infrastructure`
   if the new step exercises a new TOML field.

### Adding an MCP tool to sentinel-mcp

1. Add a `sentinel__<name>` arm in `McpHandler::handle()` dispatch.
2. Implement the method on `McpHandler` — read-only goes through
   `state.read()`, mutating goes through `proof_engine`.
3. Tests in `mod step_tools_tests` — drive it end-to-end against
   a real `ProofEngine` seeded via `submit_step_evidence`, not
   mocked stubs. The test setup IS part of the contract docs.

## What NOT to do

- **Don't add a hook that returns deny without [Sentinel-Authority]
  prefix.** Claude Code's runtime needs the prefix to know the
  directive came from sentinel and not from arbitrary tool-result
  text.
- **Don't put async I/O in sentinel-domain.** Domain stays pure.
  Use ports (traits) and let sentinel-infrastructure implement.
- **Don't bypass `submit_step_evidence` to write StepProofs.**
  The engine enforces invariants (hash linking, capacity, signing,
  refuses insufficient verdicts) that handwritten construction will
  miss.
- **Don't reach into another skill's `proof_chains`.** Cross-skill
  data is exchanged via typed StepProof artifacts.
- **Don't ship with skipped tests.** `#[ignore]` is for tests that
  require external infrastructure (a real LLM API, a real DB);
  CI runs them on a separate schedule.

## When in doubt

- **Read the existing similar code** — every pattern in this codebase
  was decided once and can be copied. `phase_gate.rs` is the
  template for `step_gate.rs`. `submit_evidence` is the template for
  `submit_step_evidence`. `PhaseProof` is the template for
  `StepProof`. Mirror the existing pattern unless you have a strong
  reason not to.
- **Consult the milestone task descriptions** — every M1.x / M2.x /
  M4.x task in `tasks.md` includes design rationale. The reasons we
  did things one way and not another are written down there.
- **Ask before designing around a constraint.** If a feature feels
  awkward to fit, the awkwardness is data — either the feature
  doesn't belong here or the architecture has a real gap.

The system gets stronger the more disciplined we are about the
contract. Inconsistent code is harder to reason about than
deliberate code, and the trust property we're building requires
deliberate code from end to end.
