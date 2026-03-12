# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- `skill_router`: activation banners now shown to user via `systemMessage` field (2026-03-12)
  - Reads `## Activation Banner` code block from `~/.claude/skills/{name}/SKILL.md`
  - Outputs banner text in `systemMessage` JSON field (visible in terminal transcript)
  - `additionalContext` still carries routing instructions for Claude (dual output)
  - Previously banners relied on Claude reading the SKILL.md and displaying them — unreliable
- `HookOutput.system_message` field in sentinel-domain (2026-03-12)
  - Serializes as `"systemMessage"` per Claude Code's JSON output schema
  - Merged via concatenation when multiple hooks produce system messages
- `sentinel steel-test` CLI subcommand for standalone Steel browser test management (2026-03-11)
  - `sentinel steel-test record` — record a passing browser test for current session
  - `sentinel steel-test check` — check if valid browser test exists for current session
- Worker hardening plan HTML (`plans/worker-hardening-plan-v1-styled.html`) with tooling versions table (2026-03-11)

### Changed
- `phase_gate`: fail-closed when `phases/` dir exists but file is missing; canonical path validation rejects `..` components, validates skill/file names are safe ASCII, resolves symlinks (2026-03-11)
- `pre_push_steel_test`: scoped Steel test requirement to repos matching project configs (not all repos with any Steel config); added Worker verification support (2026-03-11)
- `wrangler_guard`: expanded with per-repo scoping and Cloudflare API verification (2026-03-11)
- `skill_router`: fixed regex pattern match for broader skill detection (2026-03-11)
- Worker hardening plan HTML: 5 UX improvements based on Codex GPT-5.4 review — table wrapping, increased spacing, full ARIA accessibility, responsive mobile CSS at 768px, critical/info callout classes (2026-03-11)

## [0.3.0] - 2026-03-10

### Added
- `sentinel scan --sync-counts`: synchronize component counts across all marketplace text files
  - Replaces `scripts/sync-counts.js` — universal regex sweep + targeted file-specific replacements
  - `--dry-run` flag for preview mode
  - Extended counts: scripts, docs, templates, steel_tools in addition to core sentinel counts
- `sentinel scan --manifest`: generate manifest.json with SHA-256 hashes for all syncable files
  - Replaces `scripts/generate-manifest.js` — walks skills, agents, commands, scripts, templates, docs
  - Uses sha2 crate for deterministic content hashing
- `sentinel scan` CLI command: full marketplace scanner ported from Node.js `scanner.cjs` to Rust
  - `--counts-only`: output just component counts as JSON
  - `--validate`: output validation report with colored terminal output
  - `--dir <path>`: override marketplace root directory
- `sentinel-application::scanner` module: shared scanning logic used by `session_init` and `sentinel scan`
  - `parse_frontmatter()`, `extract_dependencies()`, `infer_category()`, `parse_hooks_toml()`
  - `scan_marketplace()` returns full snapshot: skills, hooks, agents, commands, MCP servers, dependency graph, validation
  - 5 categories of validation: count consistency, file cross-reference, frontmatter integrity, dependency graph, documentation counts
  - 5 unit tests for parsing and categorization
- Dashboard API endpoints on `sentinel daemon`
  - `GET /api/scan` — full marketplace snapshot (5s cache)
  - `GET /api/validation` — validation results only
  - `GET /api/counts` — component counts only
  - `POST /api/rescan` — bust cache and rescan
  - `GET /api/logs` — JSONL log reader with category/search/limit/offset filtering (9 log files)
  - `GET /api/store/browse/:owner/:repo` — browse GitHub repo for skills
  - `GET /api/store/preview/:owner/:repo/:skill` — preview SKILL.md content
  - `POST /api/store/install` — install skill from GitHub repo
  - `DELETE /api/store/uninstall/:name` — remove skill from marketplace
  - `GET /api/sentinel/sessions` — list all session summaries (reads state/*.json)
  - `GET /api/sentinel/sessions/:id` — full session state
  - `GET /api/sentinel/config` — hooks.toml + workflows.toml summary
  - `GET /api/sentinel/stats` — aggregated stats across all sessions
- Dashboard frontend switched from Express backend to sentinel daemon
  - Vite proxy forwards `/api` to `http://localhost:3001` (sentinel daemon)
  - Removed hardcoded `localhost:3001` from all React hooks and pages
  - `npm run dev` now only starts Vite (sentinel daemon runs separately)
  - `npm run dev:legacy` preserved for Express fallback
- CLAUDE.md generator: "Rust Tooling Ecosystem" section with dynamic MCP/CLI repo counts, naming conventions, and infrastructure docs
- `count_repos_with_suffix()` helper: scans `~/Documents/GitHub/` for repos matching `*-mcp-rust` / `*-cli-rust` patterns
- `ComponentCounts` now includes `mcp_repos` and `cli_repos` fields

### Changed
- Extracted counting functions from `session_init.rs` into shared `scanner` module — `count_subdirs`, `count_files_with_ext`, `count_mcp_servers`, `count_repos_with_suffix`, `count_components`
- `scripts/sync-counts.js`: replaced 7 JS counting functions with `sentinel scan --counts-only` (falls back gracefully if binary unavailable)
- `install.js`: replaced `countSentinelHooks()` and `countReposBySuffix()` with cached `sentinel scan --counts-only` call

### Fixed
- All pre-existing compiler warnings across workspace (7 unused imports/variables in domain, application, infrastructure crates)
- `verification_gate::test_prompt_injects_and_clears` flaky test: race condition with parallel tests and live sentinel sharing cooldown file — now uses process-unique temp file via env var override

## [0.2.0] - 2026-03-07

### Added
- `sentinel-mcp` crate: standalone MCP server built with Vulcan SDK
  - 8 tools: `get_proof_chain`, `get_workflow_status`, `verify_chain`, `submit_phase_complete`, `get_session_stats`, `update_step`, `get_phase_steps`, `get_workflow_progress`
  - Replaces the hand-rolled JSON-RPC MCP in `sentinel-cli/src/mcp_cmd.rs`
  - Uses Vulcan `#[tool]` / `#[tool_router]` macros for zero-boilerplate tool definitions
  - Registered as Claude Code MCP server via `claude mcp add sentinel -- sentinel-mcp`
- Real AI judge powered by rig-core (Cerebras, OpenAI, Anthropic multi-model)
- `TeammateIdle` + `TaskCompleted` hook events for agent teams
- `plan_organizer` PostToolUse hook for ExitPlanMode
- Enhanced `verification_gate` with cooldown and evidence tracking
- `receiving-code-review` skill route in `skill_router`
- `activity_tracker`, `pre_compact`, `commit_message_validator` hooks
- Enhanced `pre_push_steel_test` to detect frontend file changes

### Changed
- Switched rig-core to upstream v0.30
- Converted 5 hooks to two-phase detect→inject pattern
- Aligned 8 step definitions with SKILL.md phase structures
- Wired all 20→27 hooks into sentinel event dispatch

### Fixed
- Broadened linear skill routing to match bare "linear" keyword
- Dynamic Linear team keys from marketplace project configs
- Hardened state store + skill router always reports status
- Aligned mcp_health error schema with error_reporter expectations

<!-- generated by git-cliff -->
