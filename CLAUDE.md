# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```
cargo build --release                              Build optimized binary (requires Rust 1.87+)
cargo clippy --workspace                           Lint (pedantic + nursery enabled)
cargo test --workspace                             Run all tests
cargo test -p sentinel-cli -- test_extract_skill   Run a single test by name substring
cargo test -p sentinel-application                 Run tests for one crate
```

Release profile: LTO, single codegen unit, binary stripping, panic=abort.

## CLI Commands

```
sentinel hook --event <Event>          Process a hook event
sentinel daemon --port 3001            Start daemon with local API
sentinel verify --session <id>         Verify proof chain
sentinel scan --counts-only            Marketplace component counts
sentinel scan --sync-counts            Sync counts across marketplace files
sentinel scan --manifest               Generate manifest.json with SHA-256 hashes
sentinel stats                         Hook execution statistics
sentinel browser-test record/check     Manage browser test state
sentinel telemetry collect             Spool ledger rows for the R2 lake (LEG-259)
sentinel telemetry ship [--dry-run]    Drain the spool to R2, idempotent (LEG-260)
sentinel telemetry report [--window]   Read the R2 lake → fleet-activity report (LEG-258)
sentinel mcp                           Start MCP server over stdio
```

## Architecture

7 workspace crates following DDD/hexagonal architecture (domain has no IO dependencies):

| Crate | Binary | Purpose |
|-------|--------|---------|
| `sentinel-domain` | — | Pure business logic: proofs, workflows, evidence, hooks, routing |
| `sentinel-application` | — | Use cases: engine, classifier, gate, 85 hook modules |
| `sentinel-infrastructure` | — | IO adapters: config, state store, git, MCP transport, AI judge |
| `sentinel-graph` | — | Phase-progression engine: compiles `workflows.toml` into a durable, checkpointed langgraph StateGraph with judge-verdict interrupts and Pass/Fail routing |
| `sentinel-cli` | `sentinel` | CLI (37 top-level subcommands) + local REST API (axum) + in-repo MCP host (stdio) |
| `sentinel-git-interceptor` | `sentinel-git-interceptor` | Git shim that routes commits through sentinel gates |
| `sentinel-npx-interceptor` | `sentinel-npx-interceptor` | npx shim that routes installs through sentinel gates |

> **Note:** the standalone MCP server binary (`sentinel-mcp`, Vulcan SDK) lives in a **separate repo** (`sentinel-mcp-rust`), not in this workspace. The in-repo MCP surface is hosted by `sentinel-cli` (`mcp_cmd.rs` + `sentinel-application/mcp_handler.rs`), reachable via `sentinel mcp`.

## MCP Server Tools

The in-repo MCP host (`sentinel mcp`, defined in `crates/sentinel-cli/src/mcp_cmd.rs`) exposes 33 tools via Claude Code (`mcp__sentinel__<tool>`):

| Tool | Description |
|------|-------------|
| `get_proof_chain` | Get cryptographic proof chain for a skill execution |
| `get_workflow_status` | Current workflow state (completed/current/next phases) |
| `verify_chain` | Re-verify proof chain integrity (hash consistency) |
| `submit_phase_complete` | Submit phase completion for AI judge evaluation |
| `record_dyad_verdict` | Record implementer/reviewer/tester dyad authorization through LangGraph checkpoints |
| `get_session_stats` | Hook invocations, blocked calls, per-hook timing |
| `update_step` | Update step status within a skill phase |
| `submit_step_complete` | Seal a StepProof and durable LangGraph checkpoint together |
| `get_phase_steps` | All steps and status for a specific phase |
| `get_workflow_progress` | Full hierarchical progress (phase + step level) |
| `regenerate_claude_md` | Regenerate `~/.claude/CLAUDE.md` from template with fresh counts |
| `edit_claude_md_template` | Find-and-replace on generator template source, then auto-regenerate |
| `restart_all_mcps` | Touch all mcp-router watched binaries to trigger mass restart |
| `get_wip_snapshot` | Current WIP-by-stage snapshot (in-flight tickets per team/state + bottleneck flags) |
| `route_capability` | Query the A2 capability router to pick the best-fit agent for a unit of work |
| `delegate_codex` | Delegate an adversarial/code-reasoning task to the Codex worker model (via OpenRouter) |
| `delegate_kimi_context_scan` | Delegate a cheap large-context scan to the Kimi worker model (via OpenRouter) |
| `ba_draft` | Draft a BA recommendation and authorize its BA1/BA3/A13 structure through durable BA draft LangGraph checkpoints |
| `replay_phase` | Time-travel replay of a skill phase by forking from the checkpoint before it (first-class QA-failed re-attempt) |
| `dev_scorecard` | Per-developer scorecards (throughput, first-pass QA, consistency) from git + Linear, with attribution-divergence flagging |
| `linear_code_audit` | Flag Completed tickets with zero code evidence (commits/files) — potential false-done |
| `linear_health` | Composite 0-100 Linear health score (hygiene, structure, data quality, flow) |
| `linear_pm_audit` | PM-enforcement audit: estimate hygiene, oversized tickets, QA-failed risk, burndown, cycle-time calibration |
| `severity_scan` | Dual-LLM severity/priority judging per ticket (report-only in MCP; CLI --apply uses LangGraph checkpoint authorization for gap-fills) |
| `tokens_scan` | Token usage by Linear ticket, classified through durable token-usage LangGraph checkpoints |
| `cache_efficiency` | Prompt-cache hit-rate scan, classified through durable cache-efficiency LangGraph checkpoints |
| `cost_per_point` | Tokens and dollars per story point, classified through durable cost-per-point LangGraph checkpoints |
| `deploy_frequency` | DORA deploy cadence aggregate, classified through durable deploy-frequency LangGraph checkpoints |
| `pr_review` | PR review health and human-review coverage, classified through durable PR-review LangGraph checkpoints |
| `roi` | Claude-vs-human-team ROI, classified through durable ROI LangGraph checkpoints |
| `sla` | SLA breach aggregate, classified through durable SLA LangGraph checkpoints |
| `token_cost` | Price the SEN-7 token aggregate at API rates; cached-vs-uncached cost split |
| `eval_run` | Run BA-Eval candidate artifacts, persist EvalRunResult, and authorize the aggregate verdict through durable eval LangGraph checkpoints |

### CLAUDE.md Self-Maintenance

The global `~/.claude/CLAUDE.md` is generated from a template in `session_init.rs`:
- **Template location**: `crates/sentinel-application/src/hooks/session_init.rs` (the `format!()` string in `generate_claude_md()`)
- **Public API**: `session_init::regenerate_global_claude_md()` — re-counts components, writes fresh file
- **Public API**: `session_init::template_source_path()` — returns path to the template source file
- Template changes require rebuilding the `sentinel` binary (the template is compiled into it; the in-repo MCP host runs from `sentinel mcp`)

## Hook System

Hooks are invoked by Claude Code's runtime via `sentinel hook --event <Event>`:
- `PreToolUse` / `PostToolUse` — before/after tool execution
- `UserPromptSubmit` — when user sends a message
- `Stop` — when Claude finishes responding
- `SessionStart` / `PreCompact` — session lifecycle

85 hook modules (one `.rs` file per hook in `hooks/`). The categories below are **representative, not exhaustive** — they show a sampling of each category, not all 85:

| Category | Hooks (representative) |
|----------|-------|
| **Blocking** | `phase_gate`, `pre_push_browser_test`, `commit_message_validator`, `git_hygiene`, `pre_commit_verification`, `spec_challenge_gate`, `db_ops_gate`, `pr_merge_gate` |
| **Observational** | `commit_hygiene`, `mcp_health`, `error_reporter`, `verification_gate`, `context_monitor`, `good_citizen_observer` |
| **Routing** | `skill_router`, `skill_telemetry` |
| **Session** | `session_init`, `pre_compact`, `activity_tracker`, `execution_log` |
| **Workflow** | `phase_validator`, `plan_organizer`, `hygiene_override`, `task_completed`, `teammate_idle` |
| **Docs/Todos** | `doc_drift`, `doc_cleanup`, `todo_interceptor`, `todo_loader` |

## Key Paths

- `crates/sentinel-application/src/hooks/` — all hook implementations (one file per hook; 85 modules)
- `crates/sentinel-application/src/hooks/mod.rs` — `HOOK_NAMES` const, `GitStatusPort` trait
- `crates/sentinel-domain/src/workflow.rs` — `SkillWorkflow`, `WorkflowPhase` definitions
- `crates/sentinel-domain/src/proof.rs` — `ProofChain`, `PhaseProof`
- `crates/sentinel-cli/src/hook_cmd.rs` — hook event dispatcher
- `crates/sentinel-cli/src/api/` — local REST API (hooks, proofs, workflows, scan, store, logs, sessions)
- `crates/sentinel-cli/src/mcp_cmd.rs` — in-repo MCP host entry point (`sentinel mcp`, stdio transport, tool definitions)
- `crates/sentinel-application/src/mcp_handler.rs` — MCP tool handlers (proof/workflow/step logic behind the tools)
- `config/hooks.toml` — hook event-to-handler mapping
- `config/workflows.toml` — skill workflow step definitions
- `config/steps/` — per-skill step configs (49 skills)

## Conventions

- **No unsafe code** — `unsafe_code = "forbid"` in workspace lints
- **Clippy pedantic + nursery** — all warnings enabled except `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc`, `missing_panics_doc`
- **Domain purity** — `sentinel-domain` must never import IO crates. Use ports/traits for external dependencies (e.g., `GitStatusPort`)
- **Hook modules** — one file per hook in `hooks/`. Register in `mod.rs` and add to `HOOK_NAMES`
- **In-repo MCP host** — plain stdio JSON-RPC in `sentinel-cli/src/mcp_cmd.rs`; this workspace has **no Vulcan dependency**. The standalone Vulcan-SDK MCP server (`#[tool]`, `#[tool_router]`, `#[tool_handler]` macros, Vulcan path dep) lives in the separate `sentinel-mcp-rust` repo

## Dependencies on Other Repos

This workspace itself has no path dependency on Vulcan. The items below pertain to the **separate** `sentinel-mcp-rust` repo (the standalone Vulcan MCP server), which is what Claude Code registers:

- **Vulcan SDK**: `../vulcan-mcp-sdk-rust/crates/vulcan` — MCP server framework used by `sentinel-mcp-rust`
- **mcp-router**: wraps the `sentinel-mcp` binary (from `sentinel-mcp-rust`) for hot-reload in Claude Code
