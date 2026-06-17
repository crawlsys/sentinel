# Sentinel

**Proof-of-Work Hook Engine for Claude Code**

Sentinel is a Rust-based hook engine that enforces workflow compliance for Claude Code sessions. It powers ~90 lifecycle hooks covering skill routing, phase gates, commit hygiene, browser testing, MCP health checks, and more — all backed by cryptographic proof chains.

## Quick Install

One-command install of sentinel + the linear MCP/CLI + the marketplace:

```bash
curl -sSL https://raw.githubusercontent.com/legatus-ai/sentinel/main/scripts/bootstrap.sh | bash
```

Re-run-safe (skips anything already present). Override paths with `SENTINEL_GH_DIR`, `SENTINEL_CLAUDE_DIR`, `SENTINEL_GH_OWNER`. See [`scripts/bootstrap.sh`](scripts/bootstrap.sh) for the full step list.

## Architecture

Sentinel follows DDD / Hexagonal Architecture with 7 crates:

```
crates/
├── sentinel-domain          Pure business logic (no IO)
│   ├── proof.rs             ProofChain, PhaseProof
│   ├── workflow.rs          SkillWorkflow, WorkflowPhase
│   ├── evidence.rs          Evidence, EvidenceEntry
│   ├── hooks.rs             HookId, HookSpec, HookResult
│   ├── judge.rs             JudgeVerdict (AI judge interface)
│   ├── routing.rs           RegexRouter (skill routing)
│   ├── state.rs             SessionState
│   ├── events.rs            HookEvent, HookOutput (incl. systemMessage)
│   └── dependency.rs        Dependency graph (petgraph)
│
├── sentinel-application     Use cases & hook implementations
│   ├── engine.rs            Hook orchestration engine
│   ├── classifier.rs        Event classifier
│   ├── gate.rs              Phase gate logic (fail-closed, path validation)
│   ├── proof_engine.rs      Proof chain builder
│   ├── judge_service.rs     AI judge service (rig-core)
│   ├── scanner.rs           Marketplace scanner
│   ├── verifier.rs          Proof chain verifier
│   ├── mcp_handler.rs       MCP tool handlers
│   └── hooks/               ~90 hook modules
│
├── sentinel-infrastructure  IO adapters
│   ├── config.rs            TOML/JSON config loading
│   ├── state_store.rs       Session state persistence
│   ├── proof_store.rs       Proof chain storage
│   ├── git.rs               Git operations
│   ├── stdin.rs / stdout.rs Hook IO (Claude Code protocol)
│   ├── rig_judge.rs         AI judge adapter (Cerebras/OpenAI/Anthropic)
│   ├── anthropic.rs         Anthropic API client
│   ├── mcp_transport.rs     MCP stdio transport
│   ├── activity_log.rs      Activity logging
│   ├── error_log.rs         Error logging
│   ├── transcript.rs        Session transcript reader
│   └── ipc.rs               Daemon IPC
│
├── sentinel-cli             CLI binary (`sentinel`)
│   ├── main.rs              CLI subcommands
│   ├── mcp_cmd.rs           In-repo MCP host (`sentinel mcp`, stdio)
│   └── api/                 Dashboard REST API (axum)
│
├── sentinel-legatus         Legatus integration (consul peers, federation client)
│
├── sentinel-git-interceptor Git shim that routes commits through sentinel gates
│
└── sentinel-npx-interceptor npx shim that routes installs through sentinel gates
```

> **Note:** the standalone MCP server binary (`sentinel-mcp`, Vulcan SDK) lives in a **separate repo**
> (`sentinel-mcp-rust`), not in this workspace. The in-repo MCP surface is hosted by `sentinel-cli`
> (`mcp_cmd.rs` + `sentinel-application/mcp_handler.rs`), reachable via `sentinel mcp`.

## CLI Commands

```
sentinel daemon                Start MCP server + hook listener + dashboard API
sentinel hook --event <Event>  Process a hook event (thin client or standalone)
sentinel verify --session <id> Verify a session's proof chain
sentinel mcp                   MCP server over stdio (Claude Code connects here)
sentinel scan                  Scan marketplace, output JSON snapshot
sentinel stats                 Hook execution statistics
sentinel steel-test            Manage Steel browser test state (record/check)
```

### Scanner Flags

```
sentinel scan --counts-only    Output just component counts as JSON
sentinel scan --validate       Output validation report with colored output
sentinel scan --sync-counts    Synchronize counts across all marketplace files
sentinel scan --manifest       Generate manifest.json with SHA-256 hashes
sentinel scan --dry-run        Preview changes without writing (with --sync-counts)
sentinel scan --dir <path>     Override marketplace root directory
```

### Dashboard API

The `sentinel daemon` exposes a REST API on port 3001:

| Endpoint | Description |
|----------|-------------|
| `GET /api/scan` | Full marketplace snapshot (5s cache) |
| `GET /api/counts` | Component counts only |
| `GET /api/validation` | Validation results |
| `POST /api/rescan` | Bust cache and rescan |
| `GET /api/logs` | JSONL log reader with filtering |
| `GET /api/sentinel/sessions` | List all session summaries |
| `GET /api/sentinel/sessions/:id` | Full session state |
| `GET /api/sentinel/config` | hooks.toml + workflows.toml summary |
| `GET /api/sentinel/stats` | Aggregated stats across sessions |
| `GET /api/store/browse/:owner/:repo` | Browse GitHub repo for skills |
| `POST /api/store/install` | Install skill from GitHub |
| `DELETE /api/store/uninstall/:name` | Remove skill |

## Hooks (~90 modules)

The categories below are **representative, not exhaustive** — a sampling of each category, not all ~90.

| Category | Hooks (representative) |
|----------|-------|
| **Blocking** | `phase_gate`, `pre_push_browser_test`, `commit_message_validator`, `git_hygiene`, `pre_commit_verification`, `wrangler_guard`, `spec_challenge_gate`, `db_ops_gate`, `pr_merge_gate` |
| **Observational** | `commit_hygiene`, `mcp_health`, `error_reporter`, `verification_gate`, `evidence_collector`, `context_monitor` |
| **Reality-check** | `claim_reality_check`, `step_anomaly`, `requirements_traceability_gate`, `provenance_validate`, `good_citizen_observer` |
| **Routing** | `skill_router` (with activation banners), `skill_telemetry` |
| **Session** | `session_init`, `pre_compact`, `activity_tracker`, `execution_log` |
| **Workflow** | `phase_validator`, `plan_organizer`, `hygiene_override`, `task_completed`, `teammate_idle` |
| **Docs/Todos** | `doc_drift`, `doc_cleanup`, `todo_interceptor`, `todo_loader` |

## Configuration

```
config/
├── hooks.toml       Hook event-to-handler mapping
├── workflows.toml   Skill workflow step definitions
└── steps/           Per-skill step configs (49 skills)
```

## Key Dependencies

- **tokio** — async runtime
- **clap** — CLI framework
- **axum** — dashboard API server (with WebSocket)
- **rig-core** — multi-model AI judge (Cerebras, OpenAI, Anthropic)
- **sha2 + hmac** — cryptographic proof chains
- **petgraph** — dependency graphs
- **vulcan** — MCP server SDK (sentinel-mcp crate)

## Build

```bash
cargo build --release
```

Requires Rust 1.83+. The release profile enables LTO, single codegen unit, and binary stripping.

## Testing & pre-push hook

```bash
cargo test -p sentinel-application --lib
```

This repo ships a `.githooks/pre-push` that runs the same command and blocks the push on failure. Enable it once per clone:

```bash
git config core.hooksPath .githooks
```

The same test suite also runs on every push/PR via `.github/workflows/test.yml`. Emergency bypass (local only):

```bash
SENTINEL_SKIP_PREPUSH_TEST=1 git push ...
```

## License

MIT
