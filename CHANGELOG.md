# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

- **Dashboard domain layer (SEN-22)**: `apps/dashboard/src/domain/` now holds 8 pure-TypeScript modules (`cycle-time`, `story-point`, `dollars`, `dora`, `sla`, `roi`, `wip`, `ticket`) plus a barrel `index.ts`. Branded primitives (`Hours`, `StoryPoint`, `Dollars`, `Tokens`, `TicketIdentifier`, `Team`, `ROIRatio`, `DeployFrequency`, `ChangeFailureRate`) are constructed via validating factories (`makeStoryPoint(0)` throws; `makeTicketIdentifier("lowercase")` throws; `bucketEstimate` rounds non-canonical estimates to {1, 2, 3, 5, 8, 16} with ties rounding up — 6 → 8, 12 → 16). Pricing constants (`OPUS_RATES` / `SONNET_RATES` / `HAIKU_RATES`) mirror `crates/sentinel-domain/src/pricing.rs` exactly so server- and client-side cost calc stay in lockstep; `cost(usage, model)` falls back to opus rates for unknown model ids (conservative). DORA classifier covers all four metrics × four tiers (lead time, deploy freq, change failure rate, MTTR). 109 unit tests across 8 vitest files pin every factory + classifier branch. Zero React/Next/MUI/emotion imports in `src/domain/**` — enforced by an ESLint flat-config override using `no-restricted-imports`. `pnpm tsc --noEmit`, `pnpm lint`, `pnpm test`, and `pnpm build` all pass.

- **Nothing-aesthetic MUI theme (SEN-21)**: new `apps/dashboard/src/theme/nothing-theme.ts` exports `getNothingTheme(mode)` returning a fully-configured MUI v6 `Theme` with the Nothing design language baked in: `shadows = Array(25).fill('none')` so elevation is a no-op everywhere, `MuiButtonBase.defaultProps.disableRipple = true` for instant feedback, percussive easing `cubic-bezier(0.2, 0, 0, 1)` (no spring), pill buttons (radius 999) with Space Mono uppercase labels, technical 4px default radius elsewhere, OLED-black dark palette (`#000000` background, `#0a0a0a` paper, `#D71921` accent/error) and warm off-white light palette (`#fafaf7`), Doto display font for `h1` only, Space Grotesk for body, Space Mono for `button`/`overline`/`caption`. `MuiPaper` strips MUI v6's dark-mode elevation gradient (`backgroundImage: 'none'`); `MuiCard` defaults to `outlined`; `MuiSnackbar` is hidden globally (`display: none` — Nothing forbids toasts); `MuiSkeleton` is replaced with a `[LOADING...]` Space Mono text marker via a `::before` pseudo-element with the pulse animation suppressed. New `src/theme/ThemeRegistry.tsx` is a Client Component wrapping the app tree in `ThemeProvider` + `CssBaseline`; `app/layout.tsx` wires it in with dark mode as the default (light-mode toggle is a follow-up). `app/page.tsx` rewritten to render Card / Button / TextField / Chip / Tabs sample so visual conformance is inspectable. 5 vitest unit tests pin the shadow array, ripple disable, dark + light palette modes, and the `#D71921` error color. `pnpm tsc --noEmit`, `pnpm lint`, `pnpm test` (5/5), and `pnpm build` (171 kB First Load JS) all green.

- **PR review thoroughness metrics (SEN-18)**: new `sentinel pr-review scan` CLI subcommand walks merged PRs across the firefly-pro-gh org (firefly-pro-crm, firefly-pro-web-app, firefly-pro-api-rust, firefly-pro-routing, firefly-pro-routing-engine, firefly-pro-routing-api, firefly-pro-marketing) plus garysomerhalder/sentinel via `gh` CLI shell-out, computes review-depth metrics (avg comments per PR, time-to-first-review p50/p90 in hours), counts Codex severity blocks (CRITICAL/HIGH/MEDIUM/LOW) and CodeRabbit findings (critical / potential issue / refactor suggestion / nitpick) by parsing comment + review bodies, distinguishes bot reviewers (coderabbitai, github-actions, linear, greptile-apps, codex, renovate, dependabot, vercel) from human reviewers to compute the human-in-the-loop %, and heuristically scores CodeRabbit remediation rate via post-review commit timing. Writes `~/.claude/sentinel/metrics/pr-review.jsonl` (one row per PR with full per-PR breakdown) and `~/.claude/sentinel/metrics/pr-review-summary.json` (aggregate roll-up + per-repo). Stdout report prints global summary + per-repo breakdown. Window default 30d (configurable via `--days`). 12 unit tests pin the Codex severity parser (text + emoji forms), CodeRabbit category detection, bot vs. human classification, time-to-first-review computation, percentile aggregation (linear interpolation), `gh` JSON parsers (list + view), and end-to-end report aggregation. Live first-scan against the 8 default repos: 282 merged PRs over 30 days, 2.83 avg comments per PR, 505 CodeRabbit findings, 9.57% human-in-the-loop coverage.

- **Dashboard scaffold (SEN-20)**: new `apps/dashboard/` Next.js 15 + TypeScript + MUI v6 app inside the sentinel repo (originally specced as a separate `factory-dashboard` repo, moved into the monorepo per Gary's update). DDD/Hex layout under `src/` (`domain/`, `application/`, `ports/`, `adapters/`) plus atomic-design components (`atoms/`, `molecules/`, `organisms/`, `templates/`) — directories seeded with `.gitkeep` markers for SENTINEL-21..29 to fill in. Google Fonts (Space Grotesk, Space Mono, Doto) loaded via `next/font/google` and exposed as CSS variables. `pnpm dev` runs on port 3001; landing page renders a "DASHBOARD" header in Doto + Space Grotesk. **No Tailwind** — MUI owns all styling per Gary's instruction; `globals.css` carries only a base reset. Stub `src/lib/pricing.ts` mirrors `crates/sentinel-domain/src/pricing.rs` constants for client-side use (intentionally empty for now; SENTINEL-22 will fill it in alongside the real domain layer). Vitest configured for unit tests under `tests/`. ESLint + Prettier on Anthropic / Next defaults.

- **Cost-per-story-point analyzer (SEN-13)**: new `sentinel cost-per-point scan` CLI subcommand joins the SEN-7 `tokens-per-ticket.jsonl` rows with Linear estimates to compute `tokens_per_point = total_tokens / estimate` and `cost_per_point = cost_usd / estimate` per ticket. Tickets are bucketed to the nearest canonical Linear estimate (1, 2, 3, 5, 8, 16) — ties round up so 6 → 8 not 5, 12 → 16 not 8. Per-bucket p25/p50/p75/p90 are computed via numpy-default linear interpolation. The "estimating drift" ratio = `bucket-8 cost_p50 / bucket-2 cost_p50` flags non-linear sizing curves with a default alarm threshold of 5.0x. Outputs land at `~/.claude/sentinel/metrics/cost-per-point.jsonl` (one row per ticket-with-estimate, sorted descending by `cost_per_point`) and `~/.claude/sentinel/metrics/cost-per-point-summary.json` (totals, per-bucket stats, drift ratio + alarm flag). The Linear cache loader is permissive — accepts both top-level `[...]` arrays and `{"issues": [...]}` objects, reads the estimate from any of `estimate`/`estimateValue`/`estimate_value`/`points`/`story_points`/`storyPoints`, and silently skips issues with missing or null estimates. When zero tickets carry estimates the report says so honestly (`tickets_with_estimate: 0` + a yellow stdout warning) rather than fabricating point values — the correct answer until SEN-1's webhook capture lands an estimate-aware Linear snapshot. 9 unit tests cover bucket assignment (exact + ties + clamps), percentile computation against numpy reference values, drift alarm trigger at 6x synthetic data, empty-estimate clean report, both Linear cache shapes, and missing-input graceful degradation.

- **Cache hit rate + token efficiency tracker (SEN-14)**: new `sentinel cache scan` CLI subcommand walks `~/.claude/projects/*/` for session JSONL files, parses every `assistant` message's `usage` block, and computes the prompt-cache hit rate per session as `cache_read_input_tokens / (cache_read + input_tokens + cache_creation_input_tokens)`. Aggregates p50/p90 globally and per-project, surfaces the top-N "worst" sessions ranked by waste estimate `(1 - hit_rate) * total_tokens` (filtered to sessions with ≥50K total tokens so trivially-tiny zero-cache sessions don't dominate the list), rolls a per-day mean, and converts waste tokens to USD at the conservative Opus input rate ($15/Mtok). Writes a per-session JSONL at `~/.claude/sentinel/metrics/cache-efficiency.jsonl` (one row per session with hit rate, waste estimate, and raw token counts) plus a structured summary at `~/.claude/sentinel/metrics/cache-efficiency-summary.json` (global percentiles, per-project breakdown, daily trend, top-10 worst). Stdout report colours hit-rate %, prints the worst-N list with token volume + dollar waste, and renders a 7-day bar-chart trend. Edge cases pinned: zero-usage sessions return `None` for hit rate (no 0/0 NaN), empty percentile input returns 0.0, missing project root writes empty outputs without panicking. Walker pattern duplicated from SEN-7's `tokens.rs`; consolidation into a shared session-walker helper deferred to a follow-up. 8 unit tests.

- **Tokens-per-ticket aggregator (SEN-7)**: new `sentinel tokens scan` CLI subcommand walks `~/.claude/projects/*/` for session JSONL files, extracts the Linear ticket id from the worktree path slug first (high-confidence) and grep of the first 50 user prompts second (medium-confidence), aggregates `usage` blocks across every `assistant` message in the session, applies per-model Anthropic pricing via the new `sentinel_domain::pricing` module, and writes one `{ticket, sessions, total_input, cache_read, cache_creation, output, cost_usd, models, confidence}` row per ticket to `~/.claude/sentinel/metrics/tokens-per-ticket.jsonl`. Output is full-overwrite each scan — input session JSONLs are the source of truth so re-scanning is idempotent. Pricing tiers (Opus / Sonnet / Haiku) are hardcoded constants for now; TODO move to a TOML lookup in a follow-up. Stdout report prints total sessions, mapping coverage %, and top-N most expensive tickets. Known prefix allow-list (FPCRM, FPFIELD, FPROUTE, FPMD, FPTRIBU, LEG, COR, EXA, SYN, TES, TRI, SEN) keeps `HTTP-200`-style false positives out. 15 unit tests cover path-slug extraction, prompt grep with cap, cost computation against published rates, and an end-to-end fixture scan.

### Fixed

- **Drop malformed `task_completed` / `teammate_idle` channel events (SEN-1)**: both hooks previously fell back to placeholder strings (`"?"`, `"unknown task"`, `"unknown"`) when `input.extra` was missing required fields, then emitted those placeholders as channel notifications. The result: lead sessions saw a constant trickle of `"Task #? completed: 'unknown task' (by unknown)"` and `"Teammate 'unknown' (team: unknown) is going idle"` notifications for every malformed dispatch. Both hooks now `return HookOutput::allow()` without emitting when any of `task_id` / `task_subject` / `teammate_name` is missing or contains the literal placeholder. 4 new unit tests pin both the missing-fields and unknown-literals branches.

- **Skill router: strip injected hook context before classification (SEN-2)**: the AI classifier was routing on words appearing in hook-injected reminders rather than the actual user message. Concrete trigger from a recent session: a turn that included the hook reminder `[Worktree Cleanup] 4 stale worktree(s) found` caused the `cleanup` skill to fire even though the user said nothing about cleanup. New `strip_hook_context()` helper removes `<system-reminder>...</system-reminder>` blocks (multi-line), `<channel ...>...</channel>` blocks, and bare `[Bracketed Tag] message` reminder lines before classification. If the cleaned prompt is empty (input was 100% hook context), short-circuit to `allow()`. 5 new unit tests cover all four strip cases plus pure-user-text passthrough.

- **PR auto-monitor: suppress `git push` recommendation when no remote (SEN-3)**: the merge-to-main branch unconditionally injected `1. Push to remote: \`git push\`` even when the repo had no remote configured (e.g., a fresh repo not yet pushed to GitHub). Detect remote presence by reading `<cwd>/.git/config` directly — no subprocess, no latency — and looking for any `[remote "..."]` section. Worktrees have a `.git` *file* pointing at the main repo's `.git/config`; follow that pointer. When no remote is configured, swap step 1 with `1. Configure a remote (\`git remote add origin <url>\`) before the first push`. 2 new tests pin both branches.

### Added

- **Hook invocation telemetry (`hook_metrics` module + `sentinel stats hooks`)**: every wrapped hook call now appends a single JSON line to `~/.claude/sentinel/metrics/hook-invocations.jsonl` with `ts`, `event`, `hook`, `tool`, `session_id`, `repo_root`, `duration_us`, `outcome` (allow/block/deny/inject), and a 120-char `reason` snippet for blocks. Privacy: `tool_input` and `tool_result` are never logged. Telemetry errors are swallowed so they can't break a hook. New `sentinel stats hooks [--limit N] [--hours H]` subcommand reads the file back and prints top-N by call count, top-N by total time, an outcome breakdown, and a recent-blocks list. Dashboard API endpoints land in a follow-up PR. Phase A wiring covers the `PreToolUse` and `UserPromptSubmit` event branches (~25 hook call sites); the remaining branches migrate incrementally.

- **Bug task gate (new hook `bug_task_gate`)**: when a tool result reveals a bug — `cargo test` `test result: FAILED`, a Rust compile error with a code (`error[E0277]: …`), or a runtime `panicked at` — the hook records pending-bug state at `~/.claude/sentinel/state/pending-bug-{repo_hash}.json` and blocks subsequent mutating tools (matching the `tool_usage_gate` scope) until a `TaskCreate` or `TaskUpdate` references the bug. Subject/description must contain at least one of: `bug`, `fix`, `error`, `regression`, `broken`, `failure`, `failing`, `panic`, `crash`. State auto-clears after a 10-minute TTL so a transient signal doesn't deadlock the session. Detection is conservative on purpose: only the three high-confidence patterns above trigger, so the gate doesn't fire on routine log output that happens to contain the word "error". 16 unit tests cover signal detection, false-positive suppression, TaskCreate keyword matching, and TTL behaviour.

- **Tool usage gate broadened to mutating Bash + write MCP tools**: previously the gate enforced "sequential-thinking + TaskCreate + plan-mode + active task" only on `Edit`/`Write`. Bash and MCP tools bypassed it entirely, which let large mutations (commits, pushes, Linear writes, Doppler updates) ship without a tracked task. The gate now also covers Bash commands matching mutating prefixes (`git commit/push/merge/rebase/reset`, `gh pr create/merge`, `cargo build/run/install`, `npm/yarn/pnpm install`, `rm/mv/cp/mkdir/chmod`, `docker run/build`, `kubectl apply/delete`, output redirections `>`/`>>`) and MCP tools whose verb part doesn't contain a read-only fragment (`get`, `list`, `view`, `search`, `check`, `status`, `show`, `describe`, `health`, …). Read-only Bash (`git status/log/diff`, `ls`, `cargo check`, `gh pr view`, …) and read-only MCP tools (`mcp__linear__list_issues`, `mcp__doppler__get_secret`, …) still bypass. `cd <dir> &&` prefixes are stripped before classification so compound commands aren't mis-classified. New test coverage: 7 classifier-specific cases + 4 end-to-end gate cases.

- **Skill banner enforcement**: `skill_router` always renders an activation banner now — when a SKILL.md doesn't define a `## Activation Banner` section, the router synthesizes a fallback banner from the skill's frontmatter `description` field (with a 160-char hard cap) so the visual cue is consistent across all 76+ skills regardless of authoring discipline. `sentinel scan --validate` adds a new "Skill Banner" category that flags any SKILL.md missing an explicit `## Activation Banner` as a `warn` (not `fail`, so unmigrated skills don't break CI). Audit at write-time: only 1 of 77 skills (qdrant) currently lacks a banner.

- **Skill invocation gate (new hook `skill_invocation_gate`)**: enforces that skills detected by `skill_router` are actually invoked. When a skill is detected, `skill_router` writes a pending-skill state file (`~/.claude/sentinel/state/skill-pending-{session_hash}.json`). The new `PreToolUse` gate blocks tool calls outside an allowlist of read-only / progress-toward-load tools (Read, Glob, Grep, LSP, WebSearch/Fetch, ToolSearch, Skill, Task*, sequential-thinking) until the pending skill is invoked or its SKILL.md is read. The matching `PostToolUse` handler clears the state on `Skill(skill: "<name>")` matching the pending skill or on a `Read(...)` of its SKILL.md. State auto-clears after a 5-minute TTL so a skill the user explicitly skipped doesn't deadlock the session. 12 new unit tests cover stale TTL handling, allowlist sanity, and Windows/Unix/tilde path matching.

- **Sentinel engine version + hook count in SessionStart banner**: the `[SessionStart]` line now reads `session_id: X | engine: sentinel v0.4.0 | hooks: 54` so the running build is always visible at the top of every session. Version comes from `CARGO_PKG_VERSION` (compile-time); hook count is `HOOK_NAMES.len()` so it can't drift from the registered handlers.

- **Canonical hook output envelope (`HookEnvelope` + `HookTier`)**: new types in `sentinel-domain::events` plus `HookOutput::inject_envelope(event, envelope)` constructor. Renders as `[<HookName>] <emoji> <message>` with 🟢 Info / 🟡 Warn / 🔴 Block tier emojis so the user sees a consistent prefix across all 54 hooks instead of the ad-hoc mix of brackets, emoji, and box-drawing currently in use. Phase A only — existing `inject_context` call sites are unchanged. Migration of the 50+ legacy callers will land in follow-up tasks (one batch per hook group).

### Changed

- **Worktree/branch cleanup discipline (hygiene_reminders + worktree_reminder + pr_auto_monitor)**: surface merged `worktree-*` branches as actionable cleanup commands, both local (`git branch -d`) and remote (`git push origin --delete`). Detection runs on `Stop` (via hygiene_reminders) and on the worktree-reminder injection path. The merge-to-main detector in pr_auto_monitor now names the merged branch and lists the exact `ExitWorktree` + branch-delete commands so orphaned refs don't pile up. Required new `GitStatusPort::merged_local_branches` and `GitStatusPort::merged_remote_branches` ports.

- **task_rehydrate**: always ask before rehydrating persistent tasks from a previous session — removed the Autopilot auto-rehydrate bypass so both Autopilot and Planned modes prompt for confirmation.

- **session_init**: removed the Linear assigned-to-me cache refresh cron (every 10 min) from the CLAUDE.md template. Linear issues are now fetched on demand only. Deleted stale `~/.claude/sentinel/linear-assigned.json`.

- **Sentinel-owned paths consolidated under `~/.claude/sentinel/`**: all sentinel state and config now live under the `sentinel/` subtree so `~/.claude/` root contains only Claude Code native files. Specific moves: `sentinel-settings.json` → `sentinel/config/settings.json` (loaded via `claude --settings` flag in `claude-code-handler`); per-project config files `projects/*.md` → `sentinel/projects/`; sync marker `.last-sync-commit` → `sentinel/state/last-sync-commit`; metrics JSONL files → `sentinel/metrics/` (already done in a prior release). One-time on-disk migrations run automatically on `SessionStart` via `migrate_metrics_dir()` and new `migrate_last_sync_commit()` — no manual intervention needed on upgrade.

### Added

- **Memory dashboard proxy API (Phase 8.e)**: new `api/memory.rs` module exposes
  `GET /api/memory/stats[?project=X]` and `GET /api/memory/health`, both thin
  reqwest proxies to the memory daemon (default `http://127.0.0.1:3011`,
  overridable via `SENTINEL_MEMORY_DAEMON_URL`). Per-request timeout 3s. When
  the daemon is unreachable, returns a structured 503 with `{error, reason, hint}`
  so the dashboard Memory pane can render a "daemon down" state without
  crashing. Non-JSON or unparseable upstream bodies surface as 502. Wired
  under `/api/memory/*` (the existing `/api/memories/*` namespace stays for
  the legacy precomputed-search state files). 4 unit tests cover the default
  URL pin, env override, fallback, and the 503 path against a closed port.
  Adds `reqwest` as a direct `sentinel-cli` dep (already transitive via
  `rig-core`). Dashboard UI pane deferred — this is backend API only.
- **Hookdeck typed webhook decoders for Linear / GitHub / Vercel / Railway (Hookdeck 4b)**: new `sentinel-application::hooks::hookdeck_decoders` module turns raw JSON webhook payloads into one-line human-readable summaries the session can act on without drowning in 400-line JSON dumps. Per-source sub-modules (`linear`, `github`, `vercel`, `railway`) with per-event-type matchers — Linear decodes `Issue.update` state transitions (`FPCRM-329 moved QA Failed → QA Testing`), `Comment.create` with body excerpt (`Pedro commented on FPCRM-330: "..."`), `IssueLabel`, `Reaction`, assignee changes, priority changes, title changes. GitHub decodes `pull_request` (w/ merge detection: `PR #658 merged to main by @garysomerhalder (sha e22f87a)`), `check_run`, `check_suite`, `workflow_run`, `issue_comment` (w/ CodeRabbit batch detection), `pull_request_review`, `push` (branch + commit count + pusher), `issues`. Vercel decodes `deployment.*` lifecycle with deploy URL. Railway decodes `DEPLOY`/`ALERT` with environment + commit sha/msg. Shared fallback `[HOOKDECK:<source>] <event_type> on <resource_id>` guarantees no webhook ever surfaces as raw JSON. Comment bodies and titles are truncated and newline-stripped for safe single-line rendering. Pure library: no I/O, no hook handler, no panics. Wired into channel emission via new `channel_events::channel_event_from_webhook(source, event_type, body, extra_meta)` which builds a `ChannelEvent` with typed `summary` while preserving the original `raw` JSON under meta so callers can still drill in. 35 unit tests cover all decoder paths + fallback + glue helper (Ref Hookdeck-4b).
- **Channel-event coalescing buffer + webhook replay scaffolding (Hookdeck 4c, GS-000)**: new `sentinel_application::dedupe::Coalescer` collapses repeated webhook events with the same `(source, resource_id, event_type)` key inside a 3-second sliding quiet window, emitting a single notification annotated with `coalesce_count` instead of waking the session N times per burst. Events lacking a `meta.source` bypass the coalescer and emit immediately (no added latency for non-webhook channel events). Accompanying `sentinel_application::webhook_replay` module persists each session's last-seen webhook timestamp at `~/.claude/sentinel/state/{session_id}/last_webhook_ts.txt` and provides a pure `analyze_events` summarizer that turns a batch of decoded webhooks into a one-line catchup banner (`[HOOKDECK REPLAY] Since ...: N events — 3 CI runs, 2 Linear state changes`). 18 unit tests cover burst coalescing, sliding-window extension, non-coalescable bypass, superseded-path cleanup, force-flush, bucket-count ordering, failure-prioritized highlights, corrupt/missing marker files, and round-trip persistence — all deterministic via injected `Clock` trait.

- **`memory_extract` unified-mode capture through dual-judge gate (F1-PRE-3e, GS-65)**: when `MEMORY_ENGINE_UNIFIED=1`, the Stop hook routes flat-file memory sync (`.md` files under the memory directory) through the Memory engine's `memory_capture` MCP tool instead of upserting directly into the legacy `claude-memory` Qdrant collection. Every file now clears the dual-judge gate (Opus + Codex) before landing as an atom; rejected files still advance the sync-state so they aren't re-submitted every cron cycle. Schema mapping is lossy-but-principled — `subject`=name, `predicate`=memory_type (fallback: "describes"), `value`=description+body excerpt (500-char cap), `project`=`auto-extract`, `qualifier`=`source_file=<path>` so `memory_audit` can correlate atoms back to the source `.md`. Third and final sentinel hook in the F1-PRE-3 unification chain — alongside 3c (inject) and 3d (feedback) this completes the cutover target; F1-PRE-3f will flip the default.

- **`memory_feedback` unified-mode outcome recording (F1-PRE-3d, GS-64)**: when `MEMORY_ENGINE_UNIFIED=1`, the Stop hook classifies each injected memory into a Loop 4 outcome label — `"used"` (memory name appeared in the assistant's response), `"contradicted"` (correction phrase detected AND memory wasn't used), or `"ignored"` (neither) — and calls `memory_record_outcome(event_id, outcome)` on the Memory engine MCP (GS-63) for each. `RelevanceUpdater::apply_window` folds them into per-atom utility on the next `memory learn` cron run. Fire-and-forget per call: a single memory-mcp failure logs at WARN and moves on; the Stop hook never blocks. Mirrors the inlined stdio transport from memory_inject (F1-PRE-3c) — `sentinel-infrastructure::memory_mcp_client` tests remain the source of truth for JSON-RPC framing. Legacy boost + corrections.jsonl path preserved unchanged for the F1-PRE-3f A/B window.

- **`memory_inject` unified-mode path through the Memory engine MCP (F1-PRE-3c, GS-62)**: new opt-in code path in `hooks/memory_inject.rs` that, when `MEMORY_ENGINE_UNIFIED=1` (or `true`/`yes`/`on`), routes the UserPromptSubmit search through `memory_search` on the Memory engine's MCP server instead of the legacy `claude-memory` + `claude-sessions` Qdrant collections. Side effect (intended): every call now writes a `RetrievalEvent` per surfaced atom to `memory-retrieval-log`, which is what the Loop 4 `memory learn` batch EMA-folds into per-atom utility. Closes the F1-PRE-0 audit finding that Phase 12 shipped the Loop 4 pipeline but no production code path was writing the events. Legacy path remains the default until F1-PRE-3f cutover; this flag gives a safe A/B window. Implementation note — `sentinel-infrastructure` already has a `memory_mcp_client` helper (F1-PRE-3b) but `sentinel-application` can't depend on it (would cycle; infrastructure depends on application), so this hook inlines a ~100-line twin of the stdio transport. Tests in `sentinel-infrastructure::memory_mcp_client::tests` remain the source of truth for JSON-RPC framing; if the two copies drift, reconcile there first. 4 unit tests cover `project_from_cwd` — POSIX + Windows basenames, regex sanitisation (dots/spaces → '-'), empty-path fallback to "global", and the 128-char cap. Graceful degradation: a stalled or missing memory-mcp never blocks the prompt — logs a warning and returns no injection for that turn.

- **`sentinel_infrastructure::memory_mcp_client` — stdio client for the Memory engine MCP (F1-PRE-3b)**: thin JSON-RPC client that spawns `mcp-router --single memory-mcp` as a subprocess per call, performs the MCP handshake (`initialize` → `notifications/initialized` → `tools/call`), and returns the decoded tool payload. `MemoryMcpClient::search(query, project, top_k, session)` wraps the `memory_search` tool; every call now writes `RetrievalEvent` rows to `memory-retrieval-log` server-side (Phase 11/12's Loop 4 fuel, gated on memory-mcp @ a108f26). Configurable via `MEMORY_MCP_CMD` and `MEMORY_MCP_TIMEOUT_SECS` env vars; defaults to `mcp-router --single memory-mcp` with a 10s timeout. Intended for sentinel hooks that need to call the Memory engine without taking a direct crate dependency on `memory-application` / `memory-adapters` — preserves hexagonal boundary. 6 unit tests cover shell-split parsing, env-var config fallback, response payload extraction, error surfacing, hit deserialisation, and the spawn-failure smoke path.

### Fixed

- hooks: `tool_usage_gate` now detects plan mode from the Claude Code transcript by scanning backwards for `EnterPlanMode`/`ExitPlanMode` tool_use blocks, replacing the brittle `SENTINEL_AUTOPILOT` env-var bypass. The env var is preserved only as a last-resort fallback when no transcript path is available. See commit d5a475a.
- **`tool_usage_gate` SENTINEL_AUTOPILOT deadlock**: the plan-approval check (#3) now skips when `SENTINEL_AUTOPILOT=1`, matching the pattern already used in `pr_merge_gate`. Previously the gate demanded a plan-approved marker that could only be written when the PostToolUse dispatcher saw `EnterPlanMode` or `ExitPlanMode` fire — but when those tools aren't deferred-tool-registered in the harness session (and `CLAUDE_CODE_PLAN_MODE_REQUIRED` isn't set), the model had no path to satisfy the check, producing a hard deadlock on every Edit/Write. The other three preconditions (sequential thinking, task created, task active) still apply. Also cleaned up stale comments and a test assertion in the same file that claimed "There is no `EnterPlanMode` tool — must not reference fake tool" (contradicting the 2.1.114 audit finding that `EnterPlanMode` IS a real model-callable tool, handler `r7H`). Deny message now lists `EnterPlanMode` as the primary entry path alongside Shift+Tab, env var, `Agent(mode:"plan")`, and `--permission-mode plan`. Two new tests (`test_autopilot_bypasses_plan_gate`, `test_autopilot_does_not_bypass_task_active_check`) cover the bypass and confirm it doesn't leak to other checks; existing tests guarded with a shared `AUTOPILOT_LOCK` mutex to prevent env-var leakage in parallel test runs.

### Added

- **`sentinel break --session <id>` / `--list` / `--json`** — programmatic access to glass-break state for out-of-process consumers (Legatus Utility, dashboards). `--session <id>` targets a specific session for `--status` / `--cancel` (previously both silently used "most recently modified state file"). `--list` enumerates every session under `~/.claude/sentinel/state/` with its break state (active first, then session ID). `--json` emits a stable `BreakStatusJson` schema — `{session_id, active, reason?, started_at?, expires_at?, remaining_secs, workflow?, tools_used_count}` — for `--status`, and a JSON array of the same for `--list`. Unreadable state files are skipped rather than poisoning the whole list. The anti-AI TTY challenge remains required for break **initiation**; `--cancel` does not require a TTY since it only tightens enforcement.
- **`commit_message_validator` Linear-ref enforcement**: PreToolUse Bash hook now blocks `git commit` inside a configured project (Linear issue-prefix configured in `~/.claude/projects/*.md` frontmatter) unless the commit message references one of that project's issue prefixes (`FPCRM-123`, `FPFIELD-9`, etc.). Detection is by cwd-to-project-config token matching (file stem, `name:`, `aliases:`, `doppler_project:` — all 3+ chars). Prefixes come from `issue_prefix:` or `linear_teams[].key:` entries. Existing conventional-format check unchanged. `--amend` bypassed. 11 new unit tests cover heredoc bodies, subject/body ref matching, case-insensitive match, frontmatter tokenization, alias collection, multi-team prefix collection, and cwd substring matching. Total: 26 tests in the module, 544/544 in the application crate.
- **2.1.114 audit + `EnterPlanMode` correction**: audited sentinel against claude-code-2.1.114 via the decompiler pipeline (29,422 name mappings recovered, 2,304 first-party). Earlier this unreleased cycle I removed `EnterPlanMode` from `phase_exempt_tools` believing it to be a fake tool (absent from `sdk-tools.d.ts`) — that was wrong. `EnterPlanMode` IS a live model-callable tool in the compiled binary (handler `r7H` at decompiled line 1666), just omitted from the public type declaration in both 2.1.88 and 2.1.114. Re-added to the exempt lists in `workflow.rs` and `phase_gate.rs` with citation to the binary evidence. Flipped the regression test in `workflow.rs` to assert `EnterPlanMode` IS exempt (was asserting it must be gated). Added `EnterPlanMode` handling to the PostToolUse marker dispatcher in `hook_cmd.rs` so it marks plan-approved alongside `ExitPlanMode`. Updated the `session_init.rs` CLAUDE.md template to document `EnterPlanMode` as a real entry path, plus the new `--permission-mode plan` CLI flag discovered in 2.1.114. Full audit report at `docs/sentinel-2.1.114-audit.md`. Other confirmed 2.1.114 deltas with no sentinel impact: `Agent.mode` gained `"auto"`, `EnterWorktree` gained optional `path` field, `AgentOutput` gained `toolStats`, `BashOutput` gained `staleReadFileStateHint`, `FileEditOutput.originalFile` is now nullable. `TodoWrite`/`AskUserQuestion`/remainder of `AgentInput` byte-identical. All 27 hook events unchanged. 48/48 relevant tests pass.
- **`ConfigChange` dispatcher detects plan-mode transitions**: when Claude Code fires `ConfigChange` with `field == "permissionMode"` and `new_value == "plan"`, the dispatcher writes the plan-approved marker for the current session (same marker the `PostToolUse` arms for `EnterPlanMode`/`ExitPlanMode` write). ConfigChange is the authoritative signal since every permission-mode transition in 2.1.114 routes through config: Shift+Tab UI cycle, `--permission-mode plan` CLI flag, SDK `set_permission_mode` RPC, `EnterPlanMode` tool, agent YAML `permissionMode`. Previously sentinel only detected plan mode via two tool-specific PostToolUse arms, missing the UI and CLI paths. Defensive on payload shape: checks `field`/`permission_mode` and `new_value`/`value` naming variants.

- **`hygiene_reminders` worktree-staleness detection uses `git worktree list`**: the `[Worktree Cleanup] N stale worktree(s)` reminder previously flagged every directory under `.claude/worktrees/` as stale regardless of git registry. Produced a repeating cron warning that never cleared, and broke multi-session workflows — a parallel agent's live worktree was called "stale" by the sibling session. Now queries `git worktree list --porcelain` via a new `GitStatusPort::list_worktree_names()` method and only flags directories whose basename is NOT in git's registry. If the git query fails (empty Vec), the check is skipped entirely rather than false-flagging everything. New `list_worktree_names` helper in `sentinel-infrastructure::git`, matching port method with rationale docstring, and stub impls added to all 5 existing test-mock `GitStatusPort` implementations.

- **Standard project files via `sentinel init`**: generated `LICENSE` (MIT), `BUILDING.md` (build prereqs + path dependencies), `SECURITY.md` (vulnerability reporting policy), `.editorconfig` (UTF-8/LF/indent rules), `.gitattributes` (LF normalization + binary markers), and `rustfmt.toml` (workspace formatter config). Clears the doc-drift alert that was repeating on every session start.
- **`stop_failure` rate-limit auto-rotation**: on API `rate_limit` errors, immediately rotate the active Claude account and write a relaunch marker (`~/.claude/accounts/rate-limit-relaunch.json`) so the next session picks up cleanly instead of leaving the user stuck in a dead turn. Default cooldown 300 minutes.

### Changed

- **`pr_merge_gate`**: hard-block on `gh pr merge` / `gh pr close` softened to an "ask" permission decision. CLAUDE.md still requires explicit user confirmation, but approval in-conversation is now sufficient without a hook-level deny.
- **`tool_usage_gate` plan check**: falls back to a recent plan file when the `PLAN_MARKER` session-temp file is missing (happens for resumed sessions). Plan check now passes if `{cwd}/plans/*.md` contains a file modified within the last 7 days; marker still wins when present.
- **`tool_usage_gate` plan-file walk-up**: the fallback now walks upward from `cwd` toward the repo root checking every `plans/` dir, stopping at the first `.git` entry (file or directory, handling both normal repos and worktrees). Previously only checked `{cwd}/plans/`, which broke for sessions rooted in a worktree or subdirectory whose approved plan lived at the repo root. Capped at 10 levels; 3 new unit tests cover parent-dir discovery, `.git` boundary enforcement, and the worktree case.
- **`phase_validator`**: suppresses the "load `phases/claim.md`" warning for skills whose on-disk layout has no `phases/` directory (e.g. `todo-manager`). When phases exist, the warning now derives its first-file name from the workflow config rather than hardcoding `claim.md`.

### Added

- **`orchestration_nudge` hook (UserPromptSubmit)**: soft-nudge injector that detects three orchestration patterns in the user's prompt and suggests the right tool. (a) "in parallel" / "concurrently" / "N items" → recommend `TeamCreate` over serial Task() calls. (b) "find all" / "audit the codebase" / "where is X used" → recommend `Agent(subagent_type: "Explore")` to protect main context. (c) "implement / refactor / migrate end-to-end" → recommend invoking the matching `Skill()` explicitly. Skipped inside subagents to prevent recursive nudging. 7 unit tests cover each signal plus the subagent-skip and empty-prompt cases.
- **Severity emoji prefixes on gate/reminder messages**: hard blocks now lead with 🔴 (tool_usage_gate, db_ops_gate, doppler_auth0_gate), soft reminders lead with 🟡 (worktree_reminder, orchestration_nudge). Makes output scannable since ANSI color sequences don't render in Claude Code's markdown context injection.

### Changed

- **Phase-exempt tool lists renamed and cleaned up**: the `safe_tools` arrays in `sentinel-domain/workflow.rs` (`should_block`) and `sentinel-application/hooks/phase_gate.rs` are now called `phase_exempt_tools`, and their rationale comments explicitly describe what makes a tool exempt (no code execution, no subprocess spawn, no file writes — just discovery/metadata/plan-approval). Removed the non-existent `EnterPlanMode` entry from both lists (it never matched any real tool call, but it made the lists look authoritative about a tool that doesn't exist). Added `TodoWrite` (core Claude Code todo tool — metadata-only, was previously being gated) and `TaskStop` (agent-team metadata). Test renamed from `test_allow_safe_tools` / `test_allows_safe_tools` to `test_phase_exempt_tools_not_blocked` / `test_allows_phase_exempt_tools`, expanded to exercise all exempt names, and now includes a regression assertion that `EnterPlanMode` specifically does NOT land back in the exempt list.
- **`session_init` CLAUDE.md template no longer documents a fake tool**: the generator template (compiled into `sentinel-mcp`) referenced `EnterPlanMode` in three places — the slash-command table (`/plan` row), the "Built-in Plan Mode workflow" section, and the Planned-mode rules section. Every session regenerated `~/.claude/CLAUDE.md` from this template and propagated the fake name into the live global instructions. Template now describes the real entry mechanisms (Shift+Tab, `CLAUDE_CODE_PLAN_MODE_REQUIRED=1` env var, `Agent(mode: "plan")`) and only mentions `ExitPlanMode` for the approval step.

### Added

- **`pr_merge_gate` autopilot bypass (`SENTINEL_AUTOPILOT=1`)**: when the env var is set, `gh pr merge` / `gh pr close` no longer returns `HookOutput::ask()` (which Claude Code renders as a Yes/No dialog that halts autopilot loops). Instead, the hook injects an AUTOPILOT reminder via context and allows the command. The in-conversation confirmation rule from CLAUDE.md still applies — this only removes the harness-level dialog prompt. Three new tests cover the autopilot-on, autopilot-off-explicit, and autopilot-unset cases.

### Fixed

- **`tool_usage_gate` references fake tools in block messages**: the gate instructed Claude to use `EnterPlanMode`, `TaskCreate`, and `TaskUpdate` — but Claude Code 2.1.88's real `ToolInputSchemas` union (verified against `package/sdk-tools.d.ts` in the official npm tarball) contains only `ExitPlanMode`, `TodoWrite`, and the agent-team `Task*` family. Plan mode is entered via Shift+Tab, the `CLAUDE_CODE_PLAN_MODE_REQUIRED=1` env var, the `Agent` tool with `mode: "plan"`, or agent YAML `permissionMode: "plan"` — never by a tool named `EnterPlanMode`. Updated all three block messages to reference real mechanisms (Shift+Tab / env var / Agent mode / `ExitPlanMode` for approval; `TodoWrite` alongside `TaskCreate` for the harness-agnostic case). Updated `test_blocks_edit_without_plan_approval` to assert the new wording and explicitly reject the old fake-tool reference.
- **`worktree_reminder` now nudges `AskUserQuestion`**: the reminder that fires on `UserPromptSubmit` inside a git repo previously only mentioned `EnterWorktree`. Added a trailing instruction to use `AskUserQuestion` at decision points while working, since multi-step worktree work was routinely proceeding on assumptions.
- **`tool_usage_gate` task-creation & active-task markers recognize `TodoWrite`**: the PostToolUse dispatcher at `crates/sentinel-cli/src/hook_cmd.rs:395` only matched `TaskCreate`/`TaskUpdate` (agent-team harness tools). But core Claude Code sessions use `TodoWrite` (per `sdk-tools.d.ts` in the 2.1.88 tarball) — meaning a core session could call `TodoWrite` all day and the gate would block Edit/Write forever with "create a task". Now `TodoWrite` also triggers `mark_task_created`, and any `TodoWrite` payload containing a todo with `status: "in_progress"` also triggers `mark_task_active`. Covers both the harness and the core code paths.
- **`doc_drift` write race**: `resolve_drift_for_cwd` did a lock-free read->filter->rewrite of `~/.claude/sentinel/metrics/doc-drift.jsonl`, which could clobber concurrent appends from `write_drift_entries` in parallel sessions/threads. Both paths now take an exclusive advisory lock on a sidecar `doc-drift.jsonl.lock` (via `fs2::FileExt::lock_exclusive`) before touching the file. Previously-red 50-iteration concurrency test `test_concurrent_write_and_resolve_loses_entries` now passes.
- **Cross-session cooldown suppression in 5 hooks**: `cooldown_file()` in `context_monitor`, `commit_hygiene`, `doc_drift`, `verification_gate`, and `activity_tracker` returned a shared path `std::env::temp_dir().join("claude-<hook>-last")` with no session scoping, so Session A writing the stamp suppressed Session B running in parallel on the same machine. The path now embeds `CLAUDE_SESSION_ID` (fallback `SESSION_ID`, fallback `"default"`), giving each session its own cooldown window. Previously-red `test_cross_session_cooldown_suppression_bug` now passes.
- **`commit_hygiene` state equality gate**: the Stop-phase state payload at `commit-hygiene-{repo_hash}.json` was keyed per-repo but shared across sessions working in the same checkout, so Session A writing state suppressed Session B because the `state.cwd == cwd` gate treated the payload as its own. Added `session_id` to `CommitState`, populated via the same `CLAUDE_SESSION_ID` → `SESSION_ID` → `"default"` fallback, and tightened the gate to `state.session_id == session_id && state.cwd == cwd`. New unit test `test_state_gate_distinguishes_sessions` covers both cross-session and same-session cases.
- **`session_init` validator**: removed the `sentinel-settings.json missing` false-positive. All hook registrations live in `~/.claude/settings.json`; the separate file was never actually loaded, but the validator flagged it on every SessionStart. Validator block, watch-path entry, and the stale `CLAUDE.md` tree row all deleted.
- **`pre_commit_verification` tests**: `test_blocks_git_push_without_evidence` and `test_is_docs_only_not_commit` were non-deterministic because `is_docs_only_commit` shelled out to `git diff` against the ambient cwd. Extracted a `GitDiffRunner` trait with production (`RealGitDiff`) and test-stub impls so the tests are hermetic.

## [0.4.1] - 2026-04-16

### Fixed
- **Cross-project state leak in 4 two-phase hooks** — flat shared filenames caused last-writer-wins data loss between parallel sessions and projects
  - `verification_gate`: `unverified-claims.json` → `unverified-claims-{session_id}.json`
  - `commit_hygiene`: `commit-hygiene.json` → `commit-hygiene-{repo_hash}.json` (djb2 hash of repo root)
  - `context_monitor`: `context-zone.json` → `context-zone-{session_id}.json`
  - `activity_tracker`: `activity-summary.json` → `activity-summary-{session_id}.json`
  - `pre_compact`: updated both callsites (`read_activity_summary`, `read_context_percent`) to match new scoped filenames
  - `activity_tracker::check_elevated_context`: updated cross-hook read of context-zone to use scoped filename
  - `hygiene_reminders` (0.4.0): same fix applied — `hygiene-{repo_hash}.json` pattern

## [0.4.0] - 2026-04-16

### Added
- **`dep_check` hook**: detects outdated Rust dependencies via `cargo outdated` on `UserPromptSubmit`, injects advisory into context when stale crates are found
- **3-tier session automation system**: cron-based git hygiene, worktree cleanup, and task audit jobs auto-created on session start; sentinel hooks inject event-triggered monitors for PR/push/merge workflows
- **`tool_usage_gate` hook**: hard-blocks `Edit`/`Write` unless sequential-thinking was used, a task was created, a plan was approved, and a task is active — enforces Required Tool Usage rules from CLAUDE.md
- **6 enforcement hooks for CLAUDE.md rules**: `git_hygiene` upgraded to hard-block (was advisory), plus 5 additional rule-enforcement hooks wired at PreToolUse
- **`tool_usage_gate` + channel event emitter**: MCP push notifications via file-watch IPC channel; `mcp_health` failures pushed in real-time to active session
- Git + npx interceptors consolidated into sentinel workspace
  - DDD/Hexagonal: domain (28 git rules, 26 npx redirects), application (port traits + services), infrastructure (platform adapters)
  - `sentinel-git-interceptor` binary: blocks dangerous git commands, `--bypass` with native OS dialog
  - `sentinel-npx-interceptor` binary: redirects npx to local Rust CLIs, TOML config overrides
  - Replaces standalone `git-interceptor` and `npx-interceptor` repos
  - Smaller binaries: git 274KB (was 283KB), npx 244KB (was 269KB)
- Channel events for context threshold, build/test, and deploy notifications
  - `context_monitor` emits `context_threshold` when usage crosses 65%+
  - `build_notify` PostToolUse hook: emits `build_completed` and `deploy_completed`
  - Total channel event types: 6
- **Agent Teams guidance in CLAUDE.md template**: when to use teams vs subagents, TeamCreate workflow, optimal team sizing, sentinel hook enforcement
- **`HookOutput.system_message` field** in sentinel-domain
  - Serializes as `"systemMessage"` per Claude Code's JSON output schema
  - Merged via concatenation when multiple hooks produce system messages
- **`sentinel init` CLI subcommand** for standard project file generation
  - Generates 11 standard files: README.md, CLAUDE.md, CHANGELOG.md, LICENSE, BUILDING.md, SECURITY.md, .editorconfig, .gitattributes, .gitignore, rustfmt.toml, docs/
  - Parses `Cargo.toml` for project name, description, version, license, dependencies
  - Detects project type: MCP server (vulcan dep), CLI (clap dep), workspace, library
  - Tailored content: MCP servers get mcp-router registration docs, CLIs get install instructions
  - `--dry-run` for preview, `--force` to overwrite, `--all` for batch across `~/Documents/GitHub/`
  - Skips existing files by default; creates `docs/` subdirectory structure with `.gitkeep` files
  - 18 unit tests in `sentinel-application::project_init`
- **`session_init`**: auto-runs `project_init` on every SessionStart
  - Reports created files in startup context: `[Project Init] Auto-generated N standard file(s): ...`
  - Silent when all standard files are present; skips non-git directories
- **`sentinel steel-test` CLI subcommand** for standalone Steel browser test management
  - `sentinel steel-test record` — record a passing browser test for current session
  - `sentinel steel-test check` — check if valid browser test exists for current session
- **`native task features`**: checklist, metadata, and enrichment on `task_persist`/`task_rehydrate` — structured priority/phase/tags in `metadata`, checklist sub-items, `addBlockedBy`/`addBlocks` dependencies

### Changed
- **Skill router classifier**: switched from Anthropic provider (`ANTHROPIC_API_KEY`) to OpenRouter (`OPENROUTER_API_KEY`); bumped model to Claude Opus 4.7 (`anthropic/claude-opus-4-7`)
- **AI judge**: migrated from OpenAI direct to OpenRouter (`openai/gpt-5.4`); single `JudgeProvider` struct replacing the previous multi-model `codex` field
- **`skill_router` activation banner**: moved from `systemMessage` JSON field to `additionalContext` so the banner renders as a `<system-reminder>` visible to both the model and the user
- **rig-core**: upgraded 0.34 → 0.35
- **`break` CLI subcommand**: added `--session`, `--list`, `--json` flags for programmatic consumers
- **Channel events**: session-scoped to prevent cross-session broadcast
- **Plan system overhaul**: use Claude Code's built-in Plan Mode as authoritative
  - Removed custom `plan` skill from marketplace — `EnterPlanMode`/`ExitPlanMode` now authoritative
  - `CLAUDE_CODE_PLAN_MODE_REQUIRED=1` seeded into `~/.claude/settings.json` by install.js
  - `plan_organizer` hook: copies plan files to `~/.claude/plans/{project}/{slug}-v{N}.md` with auto-incrementing versions; extracts path from `ExitPlanMode` tool_result JSON (`data.filePath`)
  - `/plan` slash command now triggers built-in Plan Mode
- `skill_router`: activation banners injected via `additionalContext` (was `systemMessage`)
- `doc_drift`: expanded from 3 monitored files to 6 (+ BUILDING.md, LICENSE, SECURITY.md); adds "run sentinel init" batch advice when 3+ standard files are missing
- `skill_router`: added project-init routing rule with 5 patterns at priority 60
- `phase_gate`: fail-closed when `phases/` dir exists but file is missing; canonical path validation rejects `..` components, validates skill/file names, resolves symlinks
- `pre_push_steel_test`: scoped to repos matching project configs; added Worker verification support
- `wrangler_guard`: expanded with per-repo scoping and Cloudflare API verification
- Removed cloudflare deploy guard hook (replaced by wrangler_guard scoping)
- `mcp-router` tools whitelisted through doppler/auth0 gate

### Fixed
- **Cross-project task rehydration leak in `task_persist` hook**
  - `find_active_task_dir` fell back to globally most-recently-modified dir under `~/.claude/tasks/` when session_id dir was missing
  - Now strictly scoped to `~/.claude/tasks/{session_id}/`; returns `None` (safe no-op) when missing
  - Added regression tests: matching session_id wins over newer-mtime siblings, missing session returns `None`, empty session dir returns `None`
- `git_hygiene`: skip when editing files outside repo root (worktree paths)
- Channel events: session-scoped to prevent cross-session broadcast

## [0.3.0] - 2026-03-10

### Added
- **Qdrant memory system** — 6 hooks for persistent vector memory across sessions
  - `memory-inject`: searches Qdrant on every UserPromptSubmit; injects top-K relevant memories and session context as `<system-reminder>`; non-blocking with periodic re-index, temporal scoring, deduplication, feedback boost
  - `memory-extract`: indexes new memories from Stop hook; privacy tags; compact injection on PreCompact
  - `memory-feedback`: tracks memory usage, correction detection, increments `access_count` on use
  - `memory-verify`: stale memory indicators; prevents runtime nesting panic
  - `session-index`: indexes full session transcripts to Qdrant on PreCompact; filters trivial exchanges; tracks memory access
  - `VectorStorePort` + `FileSystemPort` hexagonal ports for all 6 hooks; wired through dispatcher
  - Improved sync reliability and session indexing quality; temporal scoring + deduplication
- **Persistent task system** — `task_persist` + `task_rehydrate` hooks
  - `task_persist`: writes active task state to `~/.claude/tasks/{session_id}/` on every Stop
  - `task_rehydrate`: restores tasks on SessionStart; instructs `TaskCreate` recreation
- **Account cascade hook** (`account_cascade`): unified MCP account switching — when Linear/Railway/etc. account changes, cascade to all 18 MCP servers in one operation
- **Glass break emergency workflow override**
  - `sentinel break` CLI subcommand with file-based kill switch
  - Native OS dialog for confirmation before breaking
  - Unbreakable file-based state that survives hook restarts
  - Regression tests for the three lockup scenarios
- **Autopilot / Planned mode switch system**
  - Two-mode operation: `🚀 Autopilot` (fast, autonomous) and `📋 Planned` (safe, methodical, plan-required)
  - Status indicator emoji prepended to every response
  - Welcome message shown on session init; switch via "autopilot" / "planned" commands
  - Mode-specific rules: Autopilot skips plan approval, Planned requires `ExitPlanMode` before any implementation
  - Hard-block database ops in prod regardless of mode; local db ops allowed in Autopilot
- **Security hardening**
  - Encrypted state store (ChaCha20-Poly1305) for sensitive hook state
  - Process attestation (opt-in) — verifies caller identity before executing privileged hooks
  - Audit log for all hook executions with timestamps and caller info
  - Anti-replay protection for hook inputs
  - Permission model with caller validation
  - Close MCP tool bypass; protect sentinel source repos from hook interference
  - Lock hardening: removed double file lock in `state_store::save()` that deadlocked on Windows
- **Hook event parity with Claude Code 2.1.88 / 2.1.90**
  - 10 new hook events from 2.1.88 audit: `SubagentStart`, `SubagentStop`, `TeammateIdle`, `TaskCreated`, `TaskCompleted`, `PermissionDenied`, `CwdChanged`, `PostCompact`, `PostToolUseFailure`, `Notification`
  - 9 new hook events + output fields: `defer` permission decision, `agent_id`/`agent_type` input fields, `initialUserMessage`, `watchPaths`, `PostToolUseFailure` logging
  - `file_path` typed field on `HookInput`; `PermissionDenied` on hookSpecificOutput allowlist
  - 6 new event handlers for 2.1.90 feature parity
  - `CLAUDE_ENV_FILE` support; complete `hookSpecificOutput` schema
  - `tools.listChanged` capability declared for mcp-router hot-reload
- **`worktree_reminder` hook**: injects reminder to clean up stale worktrees on UserPromptSubmit; CLAUDE.md template updated with worktree preference
- **`CLAUDE_PROJECT` context injection**: hook output includes active project name for routing
- **Configurable user name** via `~/.claude/sentinel/user.toml`
- **Linear account names** read from token store for dynamic CLAUDE.md generation
- **`sentinel scan --sync-counts`**: synchronize component counts across all marketplace text files
  - Replaces `scripts/sync-counts.js` — universal regex sweep + targeted file-specific replacements
  - `--dry-run` flag for preview mode
- **`sentinel scan --manifest`**: generate manifest.json with SHA-256 hashes for all syncable files
  - Replaces `scripts/generate-manifest.js`
- **`sentinel scan` CLI command**: full marketplace scanner ported from Node.js to Rust
  - `--counts-only`: output just component counts as JSON
  - `--validate`: output validation report with colored terminal output
- **`sentinel-application::scanner` module**: shared scanning logic
  - `parse_frontmatter()`, `extract_dependencies()`, `infer_category()`, `parse_hooks_toml()`
  - `scan_marketplace()` — full snapshot with 5 validation categories
  - 5 unit tests
- **Dashboard API endpoints on `sentinel daemon`**
  - `GET /api/scan`, `GET /api/validation`, `GET /api/counts`, `POST /api/rescan`
  - `GET /api/logs` — JSONL log reader with category/search/limit/offset (9 log files)
  - `GET /api/store/browse/:owner/:repo`, `GET /api/store/preview/:owner/:repo/:skill`
  - `POST /api/store/install`, `DELETE /api/store/uninstall/:name`
  - `GET /api/sentinel/sessions`, `GET /api/sentinel/sessions/:id`, `GET /api/sentinel/config`, `GET /api/sentinel/stats`
- Dashboard frontend switched from Express backend to sentinel daemon (Vite proxy to port 3001)
- CLAUDE.md generator: "Rust Tooling Ecosystem" section with dynamic MCP/CLI repo counts
- `count_repos_with_suffix()` helper; `ComponentCounts` includes `mcp_repos` + `cli_repos`
- **Launcher staging system** (shadow binary hot-swap)
  - `sentinel stage` queues new `sentinel-engine` binary with integrity verification
  - Launcher auto-swaps `.staged` → `sentinel-engine` on next hook invocation, zero downtime
- **Hook supervision in Rust**: process-level supervision ensures hook subprocesses are killed on timeout; hook timeout wrapper prevents Claude Code session hangs
- **CLAUDE.md management MCP tools** (3 new tools in `sentinel-mcp`)
  - `regenerate_claude_md` — re-counts all components, writes fresh CLAUDE.md from template
  - `edit_claude_md_template` — find-and-replace on generator template source, then auto-regenerates
  - `restart_all_mcps` — touches all mcp-router watched binaries for mass restart
- **Skill classification via Cerebras/OpenAI** (later superseded by OpenRouter in 0.4.0): first AI-powered classifier replacing pure regex routing

### Changed
- **Hexagonal architecture refactor** (complete port coverage across all hooks)
  - `HookContext` struct: all hooks receive `ctx` with `.fs`, `.git`, `.vector_store`, `.process` ports
  - `FileSystemPort`, `VectorStorePort`, `GitStatusPort`, `ProcessPort` defined in domain; implemented in infrastructure
  - Port traits moved from application layer to domain layer (DDD purity)
  - All D1–D10 migration waves: every hook migrated from direct I/O to `ctx.fs` / `ctx.git` / `ctx.vector_store`
  - `sentinel-domain` now has zero IO dependencies; `thiserror` removed for DDD purity
  - Domain constants module: paths, cooldown durations, skill names extracted from all hooks
  - `SessionId` newtype introduced for type-safe session scoping
- All metrics isolated under `~/.claude/sentinel/metrics/` (was scattered across `~/.claude/`)
- Hook pipeline: all blocking calls eliminated from hot path (git pull, npm version check, synchronous IO)
- Tokio runtime nesting panics fixed across Stop hook, memory_verify, and all hooks
- Skill router: pure Opus routing — all regex patterns and Cerebras/OpenAI fallbacks removed; Opus 4.6 via Anthropic (later upgraded to 4.7 via OpenRouter in 0.4.0)
- `verification_gate`: skip for content-only repos and docs-only pushes
- Todo interceptor: aligned with `TaskCreate`/`TaskUpdate` tool names (Claude Code 2.1.88 schema)
- Extracted counting functions from `session_init.rs` into shared `scanner` module
- `scripts/sync-counts.js`: replaced 7 JS counting functions with `sentinel scan --counts-only`
- `install.js`: replaced counting functions with cached `sentinel scan --counts-only` call

### Fixed
- All pre-existing compiler warnings across workspace (7 unused imports/variables)
- `verification_gate` flaky test: race condition with parallel tests — now uses process-unique temp file via env var override
- Commit hygiene tests: stabilized with env var mutex
- Pre-commit verification gate: skip for docs-only commits; diagnostic logging for worktree paths
- Platform-conditional binary names (no hardcoded `.exe`)
- Workflow registration: stop registering from casual keyword matches

## [0.2.0] - 2026-03-07

### Added
- **`sentinel-mcp` crate**: standalone MCP server built with Vulcan SDK
  - 8 tools: `get_proof_chain`, `get_workflow_status`, `verify_chain`, `submit_phase_complete`, `get_session_stats`, `update_step`, `get_phase_steps`, `get_workflow_progress`
  - Replaces the hand-rolled JSON-RPC MCP in `sentinel-cli/src/mcp_cmd.rs`
  - Uses Vulcan `#[tool]` / `#[tool_router]` macros for zero-boilerplate tool definitions
  - Registered as Claude Code MCP server via `claude mcp add sentinel -- sentinel-mcp`
- **Real AI judge** powered by rig-core (Cerebras, OpenAI, Anthropic multi-model adversarial evaluation)
- `TeammateIdle` + `TaskCompleted` hook events for agent teams
- `plan_organizer` PostToolUse hook for ExitPlanMode
- Enhanced `verification_gate` with cooldown and evidence tracking
- `receiving-code-review` skill route in `skill_router`
- `activity_tracker`, `pre_compact`, `commit_message_validator` hooks
- Enhanced `pre_push_steel_test` to detect frontend file changes
- Dynamic Linear team keys from marketplace project configs
- Linear sync in `task_completed` hook

### Changed
- Switched rig-core to upstream v0.30
- Converted 5 hooks to two-phase detect→inject pattern
- Aligned 8 step definitions with SKILL.md phase structures
- Wired all 20 → 27 hooks into sentinel event dispatch

### Fixed
- Broadened linear skill routing to match bare "linear" keyword
- Hardened state store + skill router always reports status
- Aligned mcp_health error schema with error_reporter expectations

## [0.1.0] - 2026-02-01

### Added
- **Sentinel workspace**: 5-crate DDD/hexagonal architecture
  - `sentinel-domain`: pure business logic — proofs, workflows, evidence, hooks, routing (zero IO dependencies)
  - `sentinel-application`: use cases — hook engine, classifier, phase gate, 20 hook modules
  - `sentinel-infrastructure`: IO adapters — config, state store, git, MCP transport, AI classifier
  - `sentinel-cli` (`sentinel`): CLI with 7 subcommands + dashboard REST API (axum)
  - `sentinel-mcp-cmd`: initial hand-rolled MCP server (later replaced by `sentinel-mcp` crate in 0.2.0)
- **TOML config system**: `config/hooks.toml` (hook-to-event mapping) + `config/workflows.toml` (skill workflow steps) + `config/steps/` (49 per-skill step configs)
- **Hook engine**: dispatches all hook events through `sentinel hook --event <Event>`
  - 20 initial hooks across 5 categories: blocking, observational, routing, session, workflow
  - `UserPromptSubmit`: `error_reporter`, `hygiene_override`, `todo_loader`
  - `Stop`: `execution_log`, `skill_telemetry`, `doc_cleanup`, `verification_gate`
  - `PreToolUse`/`PostToolUse`: 4 hooks
  - `SessionStart`: `session_init` with CLAUDE.md generation, sync validation, dynamic counts
- **Proof chain system**: `ProofChain` + `PhaseProof` cryptographic chaining; `submit_phase_complete` triggers AI judge evaluation; `verify_chain` checks hash consistency
- **Phase gate**: enforces skill workflow phases; blocks tools until correct phase loaded; fail-closed on missing phase files
- **Skill router**: initial AI classification (Cerebras + OpenAI fallback) replacing pure regex routing
- **Step tracking**: hierarchical phase + step progress; 120+ steps across 8 phases for Linear skill; `update_step` / `get_phase_steps` / `get_workflow_progress` MCP tools
- **Sentinel workflow definitions** for all 47 tracked skills
- **Dashboard REST API** (`sentinel daemon --port 3001`)
  - `GET /api/hooks`, `GET /api/proofs`, `GET /api/workflows` endpoints
  - WebSocket support via axum
- **`session_init` hook**: generates `~/.claude/CLAUDE.md` on every SessionStart with dynamic component counts, Vulcan/mcp-router/shadow binary docs, project file templates, hook event reference
- **`skill_telemetry` hook**: records per-skill execution timing to `~/.claude/sentinel/telemetry/`
- **`doc_cleanup` hook**: removes stale doc entries from CLAUDE.md on Stop
- **`verification_gate` hook**: injects verification reminder after tool use; enforces evidence collection before phase completion
- **`commit_hygiene` hook**: checks for unpushed commits and uncommitted changes on Stop; injects reminder
