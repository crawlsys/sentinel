# Sentinel Review

Score: 78/100

## Overall Assessment

Sentinel is a serious project with a stronger core than most Claude Code guardrail and hook systems. It is not a toy policy wrapper. It has real architecture, clear security intent, and useful workflow concepts. The main issue is that it lives directly in the hook path, so reliability flaws turn into UX-breaking failures immediately.

## What Sentinel Does Well

- It has a strong product idea: proof-of-work and workflow enforcement for Claude Code is differentiated and useful.
- The architecture is thoughtful. The split across domain, application, infrastructure, and CLI is materially better than the usual one giant hook script approach.
- It treats security as an engineering problem, not branding. Things like staged binary rollout, hash verification, lock hardening, rate limiting, and session locking show real discipline.
- The workflow and phase model is compelling. The system is trying to create enforceable process, not just passive reminders.
- There is clear evidence of adversarial thinking. A lot of the inline comments and attack-fix notes show deliberate hardening work.
- The launcher and staged binary design is good. A tiny stable launcher with hot-swappable engine binaries is the right shape for minimizing hook downtime.
- The hook output model is mostly clean and aligned to Claude's real hook schema rather than an invented abstraction.

## What Is Holding It Back

- The hook path is too fragile. If Sentinel is in the `UserPromptSubmit` and `PreToolUse` path, it must be extremely boring operationally. Right now it is still too easy for a regression to freeze or wedge the CLI.
- Logging discipline was not strict enough. A single warning on stdout is enough to corrupt Claude's hook protocol.
- There is too much work happening in latency-sensitive paths. Windows caller attestation was a good example: defensible in theory, but expensive enough to break UX in practice.
- The system still has security feature versus operational cost imbalance in places. Some checks are reasonable as optional diagnostics, but not as default synchronous behavior in core hooks.
- Windows behavior needs more first-class treatment. Sentinel cannot be works on Linux, mostly survives on Windows if the primary user is on Windows terminals and shells.
- The rollout mechanism depends on file replacement, which is valid, but on Windows that interacts badly with live process locking. That needs an explicit operational strategy, not just a happy-path swap.
- The codebase shows strong intent, but not yet enough runtime simplification. There are signs of accumulating cleverness in a place that really wants ruthless predictability.

## Concrete Issues Found In The Recent Lockup

- Tracing logs were being emitted to stdout instead of stderr, which polluted hook JSON and broke Claude's protocol.
  - Fixed in [main.rs](C:/Users/garys/Documents/GitHub/sentinel/crates/sentinel-cli/src/main.rs#L178)
- Stdin reading waited on behavior that is fragile on Windows shells and hook pipelines.
  - Fixed in [stdin.rs](C:/Users/garys/Documents/GitHub/sentinel/crates/sentinel-infrastructure/src/stdin.rs#L33)
- Windows parent-process attestation was doing too much in the hook path and could keep the process alive after output.
  - Fixed by making it opt-in in [hook_cmd.rs](C:/Users/garys/Documents/GitHub/sentinel/crates/sentinel-cli/src/hook_cmd.rs#L620)
- The staged binary was not actually being consumed because `sentinel-engine.exe` was locked by a stale running process, so Claude kept hitting the old engine.

## Scoring By Dimension

- Concept: 90
- Product value: 88
- Architecture: 84
- Security instincts: 85
- Code quality: 75
- Operational reliability: 62
- Windows ergonomics: 60
- Hook-path discipline: 58

## What Would Raise The Score Quickly

- Treat hook-path code as a realtime system. Anything non-essential should move out of the synchronous path.
- Enforce a hard rule: stdout is protocol only, always. All logs and warnings go to stderr or files.
- Add explicit hook must exit within X ms regression tests on Windows.
- Reduce synchronous subprocess use in core hooks as much as possible.
- Make rollout more robust against Windows file locking. If the engine is in use, either defer replacement cleanly or swap via versioned binaries instead of delete-and-rename.
- Add health diagnostics that are opt-in and asynchronous, not default blocking checks.
- Build more end-to-end tests around the actual launcher path, not just `cargo run` or internal code paths.

## What I Think Of Sentinel Overall

- It is substantially better than average in concept and engineering seriousness.
- It is currently weaker than it should be in reliability, which matters more here than in most projects because Sentinel sits between the user and the model.
- The project has high upside. If the runtime path becomes boring, deterministic, and fast, Sentinel can move from impressive but risky to actually strong.

## Bottom Line

Sentinel is a good system with real originality and strong security instincts. Its main weakness is not bad design but insufficient operational conservatism in the exact code path where mistakes are most expensive. Right now it feels like a promising high-ambition tool that still needs another round of simplification and reliability hardening before it is truly trustworthy as always-on infrastructure.
