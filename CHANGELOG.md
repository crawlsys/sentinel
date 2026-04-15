# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Changed
- **Plan system overhaul: use Claude Code's built-in Plan Mode** (2026-04-15)
  - Removed custom `plan` skill from marketplace — Claude Code's `EnterPlanMode`/`ExitPlanMode` now authoritative
  - Set `CLAUDE_CODE_PLAN_MODE_REQUIRED=1` in `marketplace.json` settings so install.js seeds it into `~/.claude/settings.json`
  - Rewrote `plan_organizer` hook: now actually copies plan files to `~/.claude/plans/{project}/{slug}-v{N}.md` with auto-incrementing versions (previously only injected textual instructions for Claude to do the move)
  - Extracts plan file path from `ExitPlanMode` tool_result JSON (`data.filePath`)
  - `/plan` slash command now triggers built-in Plan Mode instead of the deleted custom skill
  - Updated CLAUDE.md template: documents Plan Mode workflow, dual plan storage (Claude Code's native `{project}/plans/{slug}.md` + sentinel's archive)

### Added
- Git + npx interceptors consolidated into sentinel workspace (2026-04-15)
  - DDD/Hexagonal: domain (28 git rules, 26 npx redirects), application (port traits + services), infrastructure (platform adapters)
  - `sentinel-git-interceptor` binary: blocks dangerous git commands, `--bypass` with native OS dialog
  - `sentinel-npx-interceptor` binary: redirects npx to local Rust CLIs, TOML config overrides
  - Replaces standalone `git-interceptor` and `npx-interceptor` repos
  - Smaller binaries: git 274KB (was 283KB), npx 244KB (was 269KB)
- Channel events for context threshold, build/test, and deploy notifications (2026-04-15)
  - `context_monitor` emits `context_threshold` when usage crosses 65%+
  - `build_notify` PostToolUse hook: emits `build_completed` and `deploy_completed`
  - Total channel event types: 6
- `skill_router`: activation banners now shown to user via `systemMessage` field (2026-03-12)
  - Reads `## Activation Banner` code block from `~/.claude/skills/{name}/SKILL.md`
  - Outputs banner text in `systemMessage` JSON field (visible in terminal transcript)
  - `additionalContext` still carries routing instructions for Claude (dual output)
  - Previously banners relied on Claude reading the SKILL.md and displaying them — unreliable
- `HookOutput.system_message` field in sentinel-domain (2026-03-12)
  - Serializes as `"systemMessage"` per Claude Code's JSON output schema
  - Merged via concatenation when multiple hooks produce system messages
- `sentinel init` CLI subcommand for standard project file generation (2026-03-12)
  - Generates 11 standard files: README.md, CLAUDE.md, CHANGELOG.md, LICENSE, BUILDING.md, SECURITY.md, .editorconfig, .gitattributes, .gitignore, rustfmt.toml, docs/
  - Parses `Cargo.toml` for project name, description, version, license, dependencies
  - Detects project type: MCP server (vulcan dep), CLI (clap dep), workspace, library
  - Tailored content: MCP servers get mcp-router registration docs, CLIs get install instructions
  - `--dry-run` for preview, `--force` to overwrite, `--all` for batch across `~/Documents/GitHub/`
  - `--dir <path>` to override target directory
  - Skips existing files by default; creates `docs/` subdirectory structure with `.gitkeep` files
  - 18 unit tests in `sentinel-application::project_init`
- `session_init`: auto-runs `project_init` on every SessionStart (2026-03-12)
  - Audits current working directory for missing standard files
  - Auto-generates missing files (never overwrites existing)
  - Only runs on git repos (skips non-repo directories)
  - Reports created files in startup context: `[Project Init] Auto-generated N standard file(s): ...`
  - Silent when all standard files are present
  - 3 new tests in `session_init::tests`
- `sentinel steel-test` CLI subcommand for standalone Steel browser test management (2026-03-11)
  - `sentinel steel-test record` — record a passing browser test for current session
  - `sentinel steel-test check` — check if valid browser test exists for current session
- Worker hardening plan HTML (`plans/worker-hardening-plan-v1-styled.html`) with tooling versions table (2026-03-11)

### Changed
- `doc_drift`: expanded from 3 monitored files (README, CLAUDE.md, CHANGELOG) to 6 (+ BUILDING.md, LICENSE, SECURITY.md); adds "run sentinel init" batch advice when 3+ standard files are missing (2026-03-12)
- `skill_router`: added project-init routing rule with 5 patterns (init project, standardize files, sentinel init, create missing files, project.init) at priority 60 (2026-03-12)
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
