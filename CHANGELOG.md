# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Fixed

- **`git_hygiene` falsely blocked worktree edits from sessions with cwd on main**: when Claude's session cwd is the primary repo checkout (typically on `main`) and an Edit/Write targets a file under `.claude/worktrees/*` or `.worktrees/*`, the hook resolved `current_branch` from the session cwd rather than the target file's own repo root — so it always reported `main` and blocked the edit, even though the file lives on a feature branch in a worktree. Four edits were falsely blocked in a single turn before the bug was traced. Fix: when a target `file_path` is extracted from the hook input, resolve the effective repo via `git.repo_root(&file_path)` and call `current_branch` + `is_worktree` on that root; fall back to the session cwd only when `file_path` is absent or outside any repo. Two new regression tests (`test_worktree_edit_from_main_cwd_not_blocked` — session cwd on main but file path inside a worktree on `feat/wt`, must allow; `test_direct_main_edit_still_blocked` — session cwd and file path both on main, must still block) lock in the behaviour. All 594 sentinel-application tests pass.

### Changed

- **`session_index` hook migrated to hex ports**: the PreCompact hook that indexes session transcripts into Qdrant's `claude-sessions` collection now goes through `ctx.vector_store.upsert_points` + `ctx.fs.read_to_string` instead of constructing `reqwest::Client` and calling `std::fs::read_to_string` + `dirs::home_dir` directly. Dropped the local `QdrantConfig` struct + `load_config` helper — Qdrant config now lives entirely in the `sentinel-infrastructure` adapter, accessed only through the port. Net -40 lines in the hook, one more file off the D-batch port-migration list. 16 unit tests (including a new `InMemoryFs` stub for `parse_transcript` coverage) continue to pass.

### Added

- **Live `Active Tasks` section in generated `~/.claude/CLAUDE.md`**: new `render_tasks_section(cwd)` helper reads `~/.claude/persistent-tasks/{project_hash}/tasks.json` (same schema `task_persist.rs` writes) and renders a compact markdown table of non-completed tasks (ID, subject, status, priority, blocked-by) at the top of the generated CLAUDE.md. Auto-regenerated after every `TaskCreated` and `TaskCompleted` hook event so the snapshot is always current — no drift between what `TaskList` shows and what CLAUDE.md says is live. Pure filesystem read on the hot path; graceful empty-section fallback when no persisted tasks exist for the current project.
- **Live `Linear Assigned to You` section in generated CLAUDE.md**: new `render_linear_assigned_section()` helper reads a cache at `~/.claude/sentinel/linear-assigned.json` and renders open Linear issues assigned to the user (ticket, title, state, priority, project) as a markdown table. Linear is treated as optional first-class — if the cache is absent or has no assignments, the section is either omitted or renders a short "nothing assigned, that's fine" note ("the current work may not have a Linear ticket"). Cache hydration is out-of-band (see the new cron below) so the hot path stays zero-network.
- **Linear-assigned cache refresh cron** (every 10 minutes) in the Session Automation block of the CLAUDE.md template: dispatches an in-harness prompt that walks every Linear account in `~/.claude.json`, calls `mcp__linear__list_issues` filtered to `assignee=me` and non-completed states, merges results into `~/.claude/sentinel/linear-assigned.json` with `{updated_at, issues}` shape, then calls `mcp__sentinel__regenerate_claude_md` to refresh the rendered section. Partial data tolerated (one unreachable Linear account doesn't fail the whole refresh); empty result also written so the "nothing assigned" message stays accurate.
- **Auto-regenerate CLAUDE.md on task state changes**: `task_created.rs` and `task_completed.rs` hook handlers now call `session_init::regenerate_global_claude_md()` at the end of their `process()` (wrapped in `catch_unwind` — fire-and-forget, never blocks the hook's main path). Net effect: `TaskCreate` / `TaskUpdate(status="completed")` in any session immediately rewrites the Active Tasks table in CLAUDE.md.

### Changed

- **Blocking tasks are now MANDATORY for every piece of work, in every mode** (not just Autopilot). Rule #3 of the Required Tool Usage block in the CLAUDE.md template was strengthened: every code / config / command change needs a `TaskCreate` record, independent of whether the work has a Linear ticket (Linear is optional, tasks are not). New rule #4 added right below it: **"Good citizens fix pre-existing issues"** — when encountering a broken test, obvious typo, dead code, or near-neighbour bug while working, fix it in the same PR rather than stepping over it. "Already broken" is not a license to leave things broken.
- **Task rehydration now asks before recreating in Planned mode**: `task_rehydrate.rs` used to inject an auto-execute instruction ("Recreate these as live tasks…"). In Planned mode, it now injects a prompt to ASK Gary first ("Found N incomplete task(s) from a previous session — rehydrate them? (y/n)"); only if he says yes does recreation happen. Autopilot bypasses the ask and auto-rehydrates (explicit autonomy contract — autopilot keeps momentum, and the persisted tasks are scoped to the current project's cwd hash so there's no cross-project leak risk).
- **Autopilot directive — new "Use crons and loops for async work" subsection**: baked concrete triggers for `CronCreate` (polling CI, Linear transitions, scheduled summaries) and `/loop` (one-shot polling, test+debug+retry cycles, PR review loops) so I actually use them instead of sitting in-session with `sleep` and `gh pr checks` polls. Named the specific anti-pattern (in-session polling) so the rule self-enforces.

- **Autopilot directive tuned — four soft-spot fixes**: the Autopilot section of the CLAUDE.md template now has concrete, self-enforceable rules where before there were aspirational ones.
  - **Sequential-thinking scope**: "on every non-trivial change" was too broad and triggered the `tool_usage_gate` on trivial renames. Clarified: non-trivial = new logic, multi-file edits, security/protocol/data-shape work, anything reversible only with effort. Trivial tweaks (single-line fix, rename, typo, docs-only, revert) skip the thinking call.
  - **Memory recall — concrete triggers**: replaced "use memory before non-trivial work" (vague, unenforceable) with four named triggers: user references a prior decision, task subject names a product/domain with likely history, editing a file whose path appears in memory, hitting an unfamiliar convention.
  - **Memory storage — concrete events**: four named save triggers (user corrected approach → `feedback`, non-obvious judgement call → `project`/`feedback`, stakeholder/deadline fact → `project`, quirky external-system behaviour). Each specifies the memory type so the directive self-classifies.
  - **Steel test — concrete scope**: "run when relevant" was ignored because *relevant* was undefined. Now names the actual triggers: any edit under `client/src/**`, `components/**`, `pages/**`, `*.tsx` / `*.vue` / `*.html`, or a server route that feeds UI data. Pure backend / config / tooling / docs don't trigger.
  - **Idle floor added**: the "don't stop working" rule had no termination condition, so a naive read implied perpetual work-invention. New subsection "When idling is acceptable" names the exact state (empty TaskList + no assigned Linear + no unfinished conversation work + user signalled completion) and calls out that inventing bugs to avoid idling is the opposite of senior judgement.

- **CI workflow gated on `CI_ENABLED` repo variable**: every push has been posting a red "FAIL" check whose only annotation is `"The job was not started because recent account payments have failed or your spending limit needs to be increased"` — GitHub Actions refuses to start jobs on Free-plan private repos when Actions billing is in a failed or over-limit state. That's not actionable code signal; it's billing noise that wallpapers the commit history with false negatives. The `.github/workflows/test.yml` job now runs only when `vars.CI_ENABLED == 'true'` at the repo level, and a `workflow_dispatch` trigger lets it still be kicked by hand. Default = dormant. Re-enable via `https://github.com/garysomerhalder/sentinel/settings/variables/actions` (set `CI_ENABLED=true`) or `gh variable set CI_ENABLED --body true` once Actions billing is restored. Local `cargo test -p sentinel-application --lib` remains the source of truth (587 passing on main).

- **Expanded Autopilot scope — non-prod work is free rein, prod is still gated**: updated the `Autopilot` section of the CLAUDE.md template (`session_init.rs::generate_claude_md`) so that in Autopilot I can merge PRs, change Doppler configs, change Auth0 tenants, and run dev/staging DB ops **without asking**, as long as the target is non-production. Prod configs (`prd`/`prod`/`production` — in Doppler config names, Auth0 tenant domains, or the `production` word anywhere in the tool arguments) still require explicit approval, and prod DB ops remain a hard refuse. The "Any Mode Rules" section is now explicitly overridable by the Autopilot section above it, so Autopilot isn't fighting against the generic deny-by-default text.

- **`doppler_auth0_gate` honors `SENTINEL_AUTOPILOT=1` with a prod-config guard**: when Autopilot is on, the gate inspects `tool_input` for a `config` / `project` / `domain` / `tenant` / `name` string containing any of `prd` / `prod` / `production` (case-insensitive). Non-prod → allow; prod or no-args (conservative fallback) → keep blocking. Auth0 now has the same structure — non-prod tenant domains allowed in Autopilot, production domains always blocked. Planned-mode behaviour is unchanged: every mutation still requires an explicit override phrase or user confirmation. 8 new unit tests cover: autopilot allows non-prod Doppler, blocks prod Doppler, blocks on missing args (fallback), allows non-prod Auth0, blocks prod Auth0, blocks Auth0 with no args, plus `targets_production` matrix across PROD/prd/production/domain/dev/stg/local-dev/missing-field/None. Every pre-existing test now acquires `ENV_LOCK` and clears `SENTINEL_AUTOPILOT` at entry so running `cargo test` from inside an Autopilot shell no longer flips gate behaviour.

### Fixed

- **`pr_merge_gate` tests leaked inherited `SENTINEL_AUTOPILOT=1` env var**: `test_asks_gh_pr_merge` and `test_asks_gh_pr_close` asserted `process()` returns `Ask`, but when `SENTINEL_AUTOPILOT=1` was present in the shell that ran `cargo test` (e.g. running tests inside a Claude Code autopilot session), the gate took the autopilot branch and returned `inject_context` instead — failing both tests. Fix: acquire `ENV_LOCK` and explicitly `remove_var("SENTINEL_AUTOPILOT")` at the top of each, mirroring the pattern already used by `test_no_autopilot_env_still_asks`. Tests now pass regardless of caller env.

- **`tool_usage_gate` walk-up tests polluted by real ancestor `plans/` dirs**: `test_no_plan_file_means_no_fallback`, `test_missing_plans_dir_means_no_fallback`, and `test_stale_plan_file_does_not_satisfy` all created a `TempDir` cwd and asserted `has_recent_plan_file` returns false. But the walk-up stops only at a `.git` marker — so on any dev machine where an ancestor of `TEMP` has a `plans/` dir with recent `.md` files (e.g. `C:/Users/garys/plans/` populated by Claude Code's plan organiser), the walk found those real plans and the tests failed. Fix: seed each tempdir with an empty `.git` sentinel file so the walk-up stops at the tempdir boundary, matching production usage where the hook is always called from inside a repo.

### Added

- **CLI subcommands + MCP tools for managing `~/.claude/CLAUDE.md`**: the global CLAUDE.md has advertised three sentinel MCP tools (`regenerate_claude_md`, `edit_claude_md_template`, `restart_all_mcps`) for a while, but none were actually wired into the MCP server or the CLI — the helpers in `session_init::regenerate_global_claude_md()` + `template_source_path()` existed as dead library functions. New `crates/sentinel-cli/src/claude_md_cmd.rs` hosts the shared implementation for both surfaces:
  - `sentinel regenerate-claude-md` / `mcp__sentinel__regenerate_claude_md` — re-runs the session_init regenerate pipeline and returns `{path, bytes}`.
  - `sentinel edit-claude-md-template --find <s> --replace <s>` / `mcp__sentinel__edit_claude_md_template` — safe find-and-replace against the compiled template source (`session_init.rs`). Refuses empty `find`, identical `find`/`replace`, missing `find`, and non-unique `find` (count-then-replace pattern like the `Edit` tool's `old_string` uniqueness rule). After a successful edit, auto-invokes regenerate so the live mirror stays in sync; the compiled template only picks up the change after `cargo build --release -p sentinel` + `sentinel stage`.
  - `sentinel restart-all-mcps` / `mcp__sentinel__restart_all_mcps` — parses `~/.claude.json`, walks `mcpServers.*` for entries whose command resolves to `mcp-router --single <name>` (handles inline command and `command`+`args` shapes, with or without `.exe` suffix), resolves `<name>` on `PATH` + `~/.cargo/bin`, and bumps each binary's mtime via `File::set_modified` so mcp-router's file watcher fires `notifications/tools/list_changed`. Best-effort semantics: missing binaries are reported in a `skipped` list rather than failing the whole call. Returns `{touched, skipped, touched_count, skipped_count}`. 11 unit tests cover edit_template uniqueness edge cases, mcp-router command shape parsing, and a full restart_all_mcps round-trip (fake `HOME`, fake PATH dir, one real binary + one ghost, verifies only the real one's mtime advances).

### Changed

- **Rewrote Autopilot mode directives in the `session_init` template**: the `Autopilot` H3 subsection now specifies a *fully autonomous senior engineer* contract — keep working until the queue is drained, do not ask questions unless truly blocked, parallelize by default via agent teams and fan-out subagents, auto-invoke skills when the router detects them, use memory proactively, and only halt for the short list (prod DB/deploy, Doppler/Auth0, PR merges, destructive shared-branch git ops). Heading renamed `Fast, Smart, Autonomous` → `Fully Autonomous Senior Engineer`. Template lives in `crates/sentinel-application/src/hooks/session_init.rs::generate_claude_md()`; change persists across CLAUDE.md regeneration.

### Fixed

- **`doppler_auth0_gate` override TTL too short for batch writes (FPCRM-407)**: bumped `OVERRIDE_TTL_SECS` from 60 → 300 and added renew-on-use. Real-world batch writes (4 parallel `set_secrets` across `dev`/`stg`/`prd`/`local-dev` configs) routinely exceeded 60 s because the agent's turn dispatch between user prompt and tool invocation plus parallel MCP call latency easily totals 1–2 minutes. Now every allowed mutation rewrites the override file with the current timestamp, so subsequent writes in the same batch inherit a fresh 5-minute window — a pause of more than 5 minutes with no mutations re-engages the gate. Deny message updated to reflect the new TTL and auto-renewal behavior. 8/8 gate tests still green.

- **`commit_message_validator` ignored `cd <path> && git commit` leader (FPCRM-407)**: the hook read `input.cwd` (session cwd) to pick a project's Linear-ref rule, but Bash commands that use `cd /other/repo && git commit` commit in the target repo, not the session cwd. This mis-attributed sentinel worktree commits to firefly-pro's ruleset and demanded `FPCRM-XXX` refs on commits that belonged to a different project. Added `effective_cwd_from_command()` that parses a leading `cd <path> &&` or `cd <path> ;` (handles unquoted, single-quoted, and double-quoted paths with spaces; refuses to match non-`cd` commands like `cdk deploy`), using that as the effective cwd for project-prefix detection. Falls back to `input.cwd` when no `cd` leader exists. 8 new regression tests cover the leader parser; 36/36 commit_message_validator tests pass.

### Added

- **`doppler_auth0_gate` signed-override bypass for Doppler mutations (FPCRM-322 motivated)**: the Doppler/Auth0 gate was a hard block on every mutation tool, forcing the user to run `doppler secrets set` in their own shell. Added the same signed-token override pattern already used by hygiene + verification gates — `hygiene_override::is_doppler_override` matches explicit high-friction phrases (`override doppler`, `doppler override`, `allow doppler write`, `authorize doppler write`, and the `…writes`/`…mutation(s)` variants), writes a signed SHA-256 token under `~/.claude/sentinel/overrides/doppler-{hash}` with a 60 s TTL, and `doppler_auth0_gate::process` now consumes a `HookContext` and allows a mutation when any `doppler-*` override file under the overrides dir has a `ts` less than 60 s old. Cross-session matching intentional: MCP tool calls run in a child session with a different `session_id` from the user's main prompt session, so enforcing per-session signature equality would never match in practice; the Bash redirect guard prevents unauthorized writes to the overrides dir and the prompt phrase itself is the real security boundary. **Auth0 stays hard-blocked regardless** — the override only covers Doppler; Auth0 changes always require the user to run them himself. 8 gate unit tests pass, including explicit coverage that the override does not leak to Auth0.

- **`memory_extract` unified-mode capture through dual-judge gate (F1-PRE-3e, GS-65)**: when `MEMORY_ENGINE_UNIFIED=1`, the Stop hook routes flat-file memory sync (`.md` files under the memory directory) through the Memory engine's `memory_capture` MCP tool instead of upserting directly into the legacy `claude-memory` Qdrant collection. Every file now clears the dual-judge gate (Opus + Codex) before landing as an atom; rejected files still advance the sync-state so they aren't re-submitted every cron cycle. Schema mapping is lossy-but-principled — `subject`=name, `predicate`=memory_type (fallback: "describes"), `value`=description+body excerpt (500-char cap), `project`=`auto-extract`, `qualifier`=`source_file=<path>` so `memory_audit` can correlate atoms back to the source `.md`. Third and final sentinel hook in the F1-PRE-3 unification chain — alongside 3c (inject) and 3d (feedback) this completes the cutover target; F1-PRE-3f will flip the default.

- **`memory_feedback` unified-mode outcome recording (F1-PRE-3d, GS-64)**: when `MEMORY_ENGINE_UNIFIED=1`, the Stop hook classifies each injected memory into a Loop 4 outcome label — `"used"` (memory name appeared in the assistant's response), `"contradicted"` (correction phrase detected AND memory wasn't used), or `"ignored"` (neither) — and calls `memory_record_outcome(event_id, outcome)` on the Memory engine MCP (GS-63) for each. `RelevanceUpdater::apply_window` folds them into per-atom utility on the next `memory learn` cron run. Fire-and-forget per call: a single memory-mcp failure logs at WARN and moves on; the Stop hook never blocks. Mirrors the inlined stdio transport from memory_inject (F1-PRE-3c) — `sentinel-infrastructure::memory_mcp_client` tests remain the source of truth for JSON-RPC framing. Legacy boost + corrections.jsonl path preserved unchanged for the F1-PRE-3f A/B window.

- **`memory_inject` unified-mode path through the Memory engine MCP (F1-PRE-3c, GS-62)**: new opt-in code path in `hooks/memory_inject.rs` that, when `MEMORY_ENGINE_UNIFIED=1` (or `true`/`yes`/`on`), routes the UserPromptSubmit search through `memory_search` on the Memory engine's MCP server instead of the legacy `claude-memory` + `claude-sessions` Qdrant collections. Side effect (intended): every call now writes a `RetrievalEvent` per surfaced atom to `memory-retrieval-log`, which is what the Loop 4 `memory learn` batch EMA-folds into per-atom utility. Closes the F1-PRE-0 audit finding that Phase 12 shipped the Loop 4 pipeline but no production code path was writing the events. Legacy path remains the default until F1-PRE-3f cutover; this flag gives a safe A/B window. Implementation note — `sentinel-infrastructure` already has a `memory_mcp_client` helper (F1-PRE-3b) but `sentinel-application` can't depend on it (would cycle; infrastructure depends on application), so this hook inlines a ~100-line twin of the stdio transport. Tests in `sentinel-infrastructure::memory_mcp_client::tests` remain the source of truth for JSON-RPC framing; if the two copies drift, reconcile there first. 4 unit tests cover `project_from_cwd` — POSIX + Windows basenames, regex sanitisation (dots/spaces → '-'), empty-path fallback to "global", and the 128-char cap. Graceful degradation: a stalled or missing memory-mcp never blocks the prompt — logs a warning and returns no injection for that turn.

- **`sentinel_infrastructure::memory_mcp_client` — stdio client for the Memory engine MCP (F1-PRE-3b)**: thin JSON-RPC client that spawns `mcp-router --single memory-mcp` as a subprocess per call, performs the MCP handshake (`initialize` → `notifications/initialized` → `tools/call`), and returns the decoded tool payload. `MemoryMcpClient::search(query, project, top_k, session)` wraps the `memory_search` tool; every call now writes `RetrievalEvent` rows to `memory-retrieval-log` server-side (Phase 11/12's Loop 4 fuel, gated on memory-mcp @ a108f26). Configurable via `MEMORY_MCP_CMD` and `MEMORY_MCP_TIMEOUT_SECS` env vars; defaults to `mcp-router --single memory-mcp` with a 10s timeout. Intended for sentinel hooks that need to call the Memory engine without taking a direct crate dependency on `memory-application` / `memory-adapters` — preserves hexagonal boundary. 6 unit tests cover shell-split parsing, env-var config fallback, response payload extraction, error surfacing, hit deserialisation, and the spawn-failure smoke path.

### Fixed

- **`commit_message_validator` false-positive `personal` project detection
  on every repo under `/users/garys/`**: `cwd_matches_tokens` used
  substring `.contains()` so token `gary` (an alias in `personal.md`,
  lowercased) matched the `garys` segment in the home directory of
  every cwd on this machine — turning the Linear-ref requirement into a
  "must include `GS-XXX`" hard block on every single commit repo-wide,
  regardless of which project the cwd actually belonged to. Replaced
  with path-segment equality: cwd is normalised (`\`→`/`, lowercased,
  non-empty segments), and a token matches only if it equals one of
  those segments. Three new regression tests lock in the invariant —
  (a) Windows backslash paths match correctly, (b) token `gary` does
  NOT match segment `garys`, (c) case-insensitive segment match still
  works. Ref GS-related: unblocks commits in `hookdeck-mcp-rust`,
  `sentinel` itself, and every other repo in `~/Documents/GitHub/`
  that was being mis-attributed to `personal`. 32 tests pass in the
  module.

- **`skill_router` hook classifier init outside timeout guard (Windows schannel hang)**: `RigClassifier::from_env()` can block for several seconds on Windows while schannel loads TLS root certificates. The classifier was constructed *before* entering `tokio::time::timeout`, so the 8 s budget never covered the sync init — a slow cert load hung the hook indefinitely. Moved the init inside the timeout block and offloaded it to `tokio::task::spawn_blocking` so the async executor isn't starved; the surrounding timeout now cancels the whole (init + classify) operation if it exceeds 8 s. Regression test simulates a 30 s blocking from_env via `spawn_blocking` and asserts a 200 ms timeout fires promptly. See commit ea86616.
- **`hook-internal` startup hang on Windows (3 root causes)**: (1) `RigClassifier::from_env()` was called unconditionally in `UserPromptSubmit` — `openrouter::Client::new()` makes a blocking ~1-4s TLS/DNS network call during init (rig-core v0.35) before the 8s tokio timeout guard. Guard classifier init behind `has_prompt` check so no-prompt invocations skip it entirely. (2) Step configs for all 47 skills were loaded eagerly on every invocation — each `load_skill_steps()` call is a filesystem stat + read (~100ms/file × 47 ≈ 5s on Windows). Moved to lazy load: only loads the single active-skill file after session state is read. (3) Tokio multi-thread runtime shutdown was delayed 10-15s by reqwest connection-pool threads; added `std::process::exit(0)` after `write_hook_output` since hook processes are short-lived. Tests `test_hook_internal_exits_within_timeout` and `test_hook_stdout_is_valid_json` timeout raised to 15s on Windows (was 3s) to accommodate git subprocess latency. See commit 403a17c.

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
