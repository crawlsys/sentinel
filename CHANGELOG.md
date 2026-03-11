# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added
- `sentinel scan --sync-counts`: synchronize component counts across all marketplace text files (2026-03-10)
  - Replaces `scripts/sync-counts.js` — universal regex sweep + targeted file-specific replacements
  - `--dry-run` flag for preview mode
  - Extended counts: scripts, docs, templates, steel_tools in addition to core sentinel counts
- `sentinel scan --manifest`: generate manifest.json with SHA-256 hashes for all syncable files (2026-03-10)
  - Replaces `scripts/generate-manifest.js` — walks skills, agents, commands, scripts, templates, docs
  - Uses sha2 crate for deterministic content hashing
- `sentinel scan` CLI command: full marketplace scanner ported from Node.js `scanner.cjs` to Rust (2026-03-10)
  - `--counts-only`: output just component counts as JSON
  - `--validate`: output validation report with colored terminal output
  - `--dir <path>`: override marketplace root directory
- `sentinel-application::scanner` module: shared scanning logic used by `session_init` and `sentinel scan` (2026-03-10)
  - `parse_frontmatter()`, `extract_dependencies()`, `infer_category()`, `parse_hooks_toml()`
  - `scan_marketplace()` returns full snapshot: skills, hooks, agents, commands, MCP servers, dependency graph, validation
  - 5 categories of validation: count consistency, file cross-reference, frontmatter integrity, dependency graph, documentation counts
  - 5 unit tests for parsing and categorization
- Dashboard API endpoints on `sentinel daemon` (2026-03-10)
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
- Dashboard frontend switched from Express backend to sentinel daemon (2026-03-10)
  - Vite proxy forwards `/api` to `http://localhost:3001` (sentinel daemon)
  - Removed hardcoded `localhost:3001` from all React hooks and pages
  - `npm run dev` now only starts Vite (sentinel daemon runs separately)
  - `npm run dev:legacy` preserved for Express fallback
- CLAUDE.md generator: "Rust Tooling Ecosystem" section with dynamic MCP/CLI repo counts, naming conventions, and infrastructure docs (2026-03-10)
- `count_repos_with_suffix()` helper: scans `~/Documents/GitHub/` for repos matching `*-mcp-rust` / `*-cli-rust` patterns
- `ComponentCounts` now includes `mcp_repos` and `cli_repos` fields

### Changed
- Extracted counting functions from `session_init.rs` into shared `scanner` module — `count_subdirs`, `count_files_with_ext`, `count_mcp_servers`, `count_repos_with_suffix`, `count_components`
- `scripts/sync-counts.js`: replaced 7 JS counting functions with `sentinel scan --counts-only` (falls back gracefully if binary unavailable)
- `install.js`: replaced `countSentinelHooks()` and `countReposBySuffix()` with cached `sentinel scan --counts-only` call

### Fixed
- All pre-existing compiler warnings across workspace (7 unused imports/variables in domain, application, infrastructure crates)
- `verification_gate::test_prompt_injects_and_clears` flaky test: race condition with parallel tests and live sentinel sharing cooldown file — now uses process-unique temp file via env var override
- `sentinel-mcp` crate: standalone MCP server built with Vulcan SDK (2026-03-07)
  - 8 tools: `get_proof_chain`, `get_workflow_status`, `verify_chain`, `submit_phase_complete`, `get_session_stats`, `update_step`, `get_phase_steps`, `get_workflow_progress`
  - Replaces the hand-rolled JSON-RPC MCP in `sentinel-cli/src/mcp_cmd.rs`
  - Uses Vulcan `#[tool]` / `#[tool_router]` macros for zero-boilerplate tool definitions
  - Registered as Claude Code MCP server via `claude mcp add sentinel -- sentinel-mcp`
