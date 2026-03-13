# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```
cargo build --release                              Build optimized binary (requires Rust 1.83+)
cargo clippy --workspace                           Lint (pedantic + nursery enabled)
cargo test --workspace                             Run all tests
cargo test -p sentinel-cli -- test_extract_skill   Run a single test by name substring
cargo test -p sentinel-application                 Run tests for one crate
```

Release profile: LTO, single codegen unit, binary stripping, panic=abort.

## CLI Commands

```
sentinel hook --event <Event>          Process a hook event
sentinel daemon --port 3001            Start daemon with dashboard API
sentinel verify --session <id>         Verify proof chain
sentinel scan --counts-only            Marketplace component counts
sentinel scan --sync-counts            Sync counts across marketplace files
sentinel scan --manifest               Generate manifest.json with SHA-256 hashes
sentinel stats                         Hook execution statistics
sentinel steel-test record/check       Manage Steel browser test state
sentinel mcp                           Start MCP server over stdio
```

## Architecture

5 crates following DDD/hexagonal architecture (domain has no IO dependencies):

| Crate | Binary | Purpose |
|-------|--------|---------|
| `sentinel-domain` | — | Pure business logic: proofs, workflows, evidence, hooks, routing |
| `sentinel-application` | — | Use cases: engine, classifier, gate, 27 hook modules |
| `sentinel-infrastructure` | — | IO adapters: config, state store, git, MCP transport, AI judge |
| `sentinel-cli` | `sentinel` | CLI (7 subcommands) + dashboard REST API (axum) |
| `sentinel-mcp` | `sentinel-mcp` | Standalone MCP server (Vulcan SDK) — 11 tools |

## MCP Server Tools

`sentinel-mcp` exposes 11 tools via Claude Code (`mcp__sentinel__<tool>`):

| Tool | Description |
|------|-------------|
| `get_proof_chain` | Get cryptographic proof chain for a skill execution |
| `get_workflow_status` | Current workflow state (completed/current/next phases) |
| `verify_chain` | Re-verify proof chain integrity (hash consistency) |
| `submit_phase_complete` | Submit phase completion for AI judge evaluation |
| `get_session_stats` | Hook invocations, blocked calls, per-hook timing |
| `update_step` | Update step status within a skill phase |
| `get_phase_steps` | All steps and status for a specific phase |
| `get_workflow_progress` | Full hierarchical progress (phase + step level) |
| `regenerate_claude_md` | Regenerate `~/.claude/CLAUDE.md` from template with fresh counts |
| `edit_claude_md_template` | Find-and-replace on generator template source, then auto-regenerate |
| `restart_all_mcps` | Touch all mcp-router watched binaries to trigger mass restart |

### CLAUDE.md Self-Maintenance

The global `~/.claude/CLAUDE.md` is generated from a template in `session_init.rs`:
- **Template location**: `crates/sentinel-application/src/hooks/session_init.rs` (the `format!()` string in `generate_claude_md()`)
- **Public API**: `session_init::regenerate_global_claude_md()` — re-counts components, writes fresh file
- **Public API**: `session_init::template_source_path()` — returns path to the template source file
- Template changes require rebuilding `sentinel-mcp` (template is compiled into the binary)

## Hook System

Hooks are invoked by Claude Code's runtime via `sentinel hook --event <Event>`:
- `PreToolUse` / `PostToolUse` — before/after tool execution
- `UserPromptSubmit` — when user sends a message
- `Stop` — when Claude finishes responding
- `SessionStart` / `PreCompact` — session lifecycle

27 hooks in 5 categories:

| Category | Hooks |
|----------|-------|
| **Blocking** | `phase_gate`, `pre_push_steel_test`, `commit_message_validator`, `git_hygiene`, `pre_commit_verification`, `wrangler_guard` |
| **Observational** | `commit_hygiene`, `mcp_health`, `error_reporter`, `verification_gate`, `evidence_collector`, `context_monitor` |
| **Routing** | `skill_router`, `skill_telemetry` |
| **Session** | `session_init`, `pre_compact`, `activity_tracker`, `execution_log` |
| **Workflow** | `phase_validator`, `plan_organizer`, `hygiene_override`, `task_completed`, `teammate_idle` |
| **Docs/Todos** | `doc_drift`, `doc_cleanup`, `todo_interceptor`, `todo_loader` |

## Key Paths

- `crates/sentinel-application/src/hooks/` — all 27 hook implementations (one file per hook)
- `crates/sentinel-application/src/hooks/mod.rs` — `HOOK_NAMES` const, `GitStatusPort` trait
- `crates/sentinel-domain/src/workflow.rs` — `SkillWorkflow`, `WorkflowPhase` definitions
- `crates/sentinel-domain/src/proof.rs` — `ProofChain`, `PhaseProof`
- `crates/sentinel-cli/src/hook_cmd.rs` — hook event dispatcher
- `crates/sentinel-cli/src/api/` — dashboard REST API (hooks, proofs, workflows, scan, store, logs, sessions)
- `crates/sentinel-mcp/src/main.rs` — MCP server entry point
- `config/hooks.toml` — hook event-to-handler mapping
- `config/workflows.toml` — skill workflow step definitions
- `config/steps/` — per-skill step configs (49 skills)

## Conventions

- **No unsafe code** — `unsafe_code = "forbid"` in workspace lints
- **Clippy pedantic + nursery** — all warnings enabled except `module_name_repetitions`, `must_use_candidate`, `missing_errors_doc`, `missing_panics_doc`
- **Domain purity** — `sentinel-domain` must never import IO crates. Use ports/traits for external dependencies (e.g., `GitStatusPort`)
- **Hook modules** — one file per hook in `hooks/`. Register in `mod.rs` and add to `HOOK_NAMES`
- **MCP server** — built with Vulcan SDK (`#[tool]`, `#[tool_router]`, `#[tool_handler]` macros). Depends on Vulcan via path: `../vulcan-mcp-sdk-rust/crates/vulcan`

## Dependencies on Other Repos

- **Vulcan SDK**: `../vulcan-mcp-sdk-rust/crates/vulcan` — MCP server framework
- **mcp-router**: wraps `sentinel-mcp` binary for hot-reload in Claude Code
