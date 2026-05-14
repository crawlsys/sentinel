//! `sentinel project init` — scaffold `.sentinel/` in a repo (M9.1, task #65).
//!
//! Companion to `sentinel init` (which generates user-facing repo files
//! like README.md / LICENSE / CHANGELOG.md). This one creates sentinel's
//! **own** repo-local state directory: tickets, plans, handovers,
//! lessons, proof-chains. The directory is git-tracked — these are
//! artifacts that travel with the code, in contrast to the global
//! `~/.claude/sentinel/` state which is per-machine.
//!
//! ## Layout created
//!
//! ```
//! .sentinel/
//! ├── README.md            # Convention doc
//! ├── config.json          # Per-repo sentinel config (skills enabled, etc.)
//! ├── tickets/             # Local-first ticket store (T-001.json)
//! │   └── README.md
//! ├── plans/               # /plan output with code
//! │   └── README.md
//! ├── handovers/           # Per-session summaries
//! │   └── README.md
//! ├── lessons/             # Project-specific lessons learned
//! │   └── README.md
//! └── proof-chains/        # Repo-local cryptographic chains
//!     └── README.md
//! ```
//!
//! ## Idempotency
//!
//! Re-running `sentinel project init` on a repo that already has
//! `.sentinel/` is a no-op: existing files are NOT overwritten unless
//! `--force` is passed. Missing files are filled in. This makes it
//! safe to run in CI on every clone (e.g. as a post-checkout hook) and
//! safe to run after pulling someone else's `.sentinel/` for the first
//! time (the existing files stay, the missing ones get scaffolded).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Run `sentinel project init`. `dir` defaults to current working
/// directory. `force` overwrites existing files (otherwise they're
/// preserved). `dry_run` reports what would happen without touching
/// the filesystem.
pub fn run(dir: Option<PathBuf>, force: bool, dry_run: bool) -> Result<()> {
    let repo_root = match dir {
        Some(d) => d,
        None => std::env::current_dir().context("could not resolve current directory")?,
    };
    let sentinel_dir = repo_root.join(".sentinel");

    let mut report = Report::default();

    // Top-level files
    write_or_skip(
        &sentinel_dir.join("README.md"),
        ROOT_README,
        force,
        dry_run,
        &mut report,
    )?;
    write_or_skip(
        &sentinel_dir.join("config.json"),
        DEFAULT_CONFIG_JSON,
        force,
        dry_run,
        &mut report,
    )?;

    // Subdirectories with their READMEs
    for (subdir, readme_body) in SUBDIRS {
        let dir_path = sentinel_dir.join(subdir);
        ensure_dir(&dir_path, dry_run, &mut report)?;
        write_or_skip(
            &dir_path.join("README.md"),
            readme_body,
            force,
            dry_run,
            &mut report,
        )?;
    }

    report.print(&sentinel_dir, dry_run);
    Ok(())
}

#[derive(Debug, Default)]
struct Report {
    created: Vec<PathBuf>,
    overwrote: Vec<PathBuf>,
    skipped_existing: Vec<PathBuf>,
}

impl Report {
    fn print(&self, root: &Path, dry_run: bool) {
        let prefix = if dry_run { "Would " } else { "" };
        println!(
            "{prefix}initialize sentinel project state at {}:",
            root.display()
        );
        if !self.created.is_empty() {
            let verb = if dry_run { "create" } else { "Created" };
            println!("  {verb}:");
            for p in &self.created {
                println!(
                    "    {}",
                    p.strip_prefix(root).map(|s| s.display().to_string()).unwrap_or_else(|_| p.display().to_string())
                );
            }
        }
        if !self.overwrote.is_empty() {
            let verb = if dry_run { "overwrite" } else { "Overwrote" };
            println!("  {verb} (--force):");
            for p in &self.overwrote {
                println!(
                    "    {}",
                    p.strip_prefix(root).map(|s| s.display().to_string()).unwrap_or_else(|_| p.display().to_string())
                );
            }
        }
        if !self.skipped_existing.is_empty() {
            let verb = if dry_run { "skip" } else { "Skipped" };
            println!("  {verb} (already present, pass --force to overwrite):");
            for p in &self.skipped_existing {
                println!(
                    "    {}",
                    p.strip_prefix(root).map(|s| s.display().to_string()).unwrap_or_else(|_| p.display().to_string())
                );
            }
        }
        if self.created.is_empty() && self.overwrote.is_empty() && self.skipped_existing.is_empty() {
            println!("  (nothing to do)");
        }
        if dry_run {
            println!("\nDry-run only — re-run without --dry-run to apply.");
        }
    }
}

fn ensure_dir(path: &Path, dry_run: bool, report: &mut Report) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if dry_run {
        report.created.push(path.to_path_buf());
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("create_dir_all({})", path.display()))?;
    report.created.push(path.to_path_buf());
    Ok(())
}

fn write_or_skip(
    path: &Path,
    body: &str,
    force: bool,
    dry_run: bool,
    report: &mut Report,
) -> Result<()> {
    let exists = path.exists();
    if exists && !force {
        report.skipped_existing.push(path.to_path_buf());
        return Ok(());
    }
    if dry_run {
        if exists {
            report.overwrote.push(path.to_path_buf());
        } else {
            report.created.push(path.to_path_buf());
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create_dir_all({})", parent.display()))?;
    }
    std::fs::write(path, body)
        .with_context(|| format!("write({})", path.display()))?;
    if exists {
        report.overwrote.push(path.to_path_buf());
    } else {
        report.created.push(path.to_path_buf());
    }
    Ok(())
}

/// Subdirectory list. Each entry is `(name, README body)`. Order
/// matters only for the dry-run report — the directories themselves
/// have no required creation order.
const SUBDIRS: &[(&str, &str)] = &[
    ("tickets", TICKETS_README),
    ("plans", PLANS_README),
    ("handovers", HANDOVERS_README),
    ("lessons", LESSONS_README),
    ("proof-chains", PROOF_CHAINS_README),
];

const ROOT_README: &str = r#"# `.sentinel/`

Repo-local sentinel state. **Checked into git** — these artifacts travel with the code.

## Contents

| Subdirectory | Purpose | Key files |
|--------------|---------|-----------|
| `tickets/`        | Local-first ticket store (alternative / mirror of Linear) | `T-001.json` per ticket |
| `plans/`          | `/plan` output that should live with the code | `<slug>.md` per plan |
| `handovers/`      | Per-session summaries the next session can read | `YYYY-MM-DD-<slug>.md` |
| `lessons/`        | Project-specific lessons learned by the agent | `L-001.json` per lesson |
| `proof-chains/`   | Repo-local cryptographic execution chains | `<session_id>.json` |

## Why repo-local

Global sentinel state at `~/.claude/sentinel/` is per-machine. That doesn't travel with the code: a new clone, a fresh developer, or a worktree on a different machine starts blank. Repo-local state in `.sentinel/` solves this — anything important enough to survive a `git clone` lives here.

## Relationship to `~/.claude/sentinel/`

| Belongs in `~/.claude/sentinel/` (per-machine) | Belongs in `.sentinel/` (repo-local) |
|------------------------------------------------|--------------------------------------|
| MCP server configs                             | Tickets specific to this repo's work |
| Doppler tokens (env-backed)                    | Plans for this repo's features |
| Per-machine session state                      | Lessons the agent learned about this repo |
| Activity/telemetry logs                        | Handover notes for the next session |
| Skill registry                                 | Proof chains of this repo's executions |

If you can answer "yes" to *"a new developer cloning fresh should see this"*, it goes here. If "no, it's part of my local setup," it goes in `~/.claude/sentinel/`.

## `config.json`

Per-repo sentinel config. Today it's a minimal stub:

```json
{
  "version": 1,
  "skills_enabled": ["linear", "git", "deploy"],
  "default_judge_model": null
}
```

`null` means "inherit from `~/.claude/sentinel/config/...`" or the project default. Fields are additive: omit any field to fall back to global default.

## Scaffolded by

`sentinel project init` — idempotent, missing-only by default, `--force` to overwrite. Re-run safely after pulling someone else's `.sentinel/`.
"#;

const DEFAULT_CONFIG_JSON: &str = r#"{
  "version": 1,
  "skills_enabled": null,
  "default_judge_model": null,
  "comment": "Fields with null inherit from global ~/.claude/sentinel/. Override here per-repo as needed."
}
"#;

const TICKETS_README: &str = r#"# `.sentinel/tickets/`

Local-first ticket store. Each ticket is one JSON file named `T-<NNN>.json`.

## Schema

```json
{
  "id": "T-001",
  "title": "Wire up sentinel project init",
  "type": "feature",
  "status": "in_progress",
  "phase": "implementation",
  "order": 1,
  "description": "Multi-line description...",
  "created_at": "2026-05-14T10:00:00Z",
  "updated_at": "2026-05-14T12:30:00Z",
  "completed_at": null,
  "blocked_by": ["T-000"],
  "parent_ticket": null,
  "linear_issue_id": null,
  "linear_url": null
}
```

`type`: feature, bug, chore, spike, docs, refactor.
`status`: backlog, in_progress, review, qa, completed, canceled.
`phase`: free-form workflow stage (e.g. "implementation", "qa", "deploy").

## Linear sync

When `linear_issue_id` is set, the linear skill keeps this ticket and the Linear issue in sync (state transitions, comments). For repos that don't use Linear, tickets are pure local-first — agents and humans both read/write them via filesystem ops.

Sync direction:

- **Local → Linear**: `mcp__linear__update_issue` mirrors changes.
- **Linear → Local**: `sentinel project sync linear` (TBD — not built yet) pulls remote changes.

Until the sync command lands, local edits dominate. Don't manually edit JSON to match a stale Linear state — just let the linear skill update both.

## Why JSON, not TOML

Tickets are read/written more often than they're hand-edited. JSON is universally machine-parseable and the format every IDE highlights without plugin. The `T-NNN` filename scheme keeps ordering legible without a separate index.

## Naming

`T-001`, `T-002`, ... — zero-padded to 3 digits, monotonic. Padding to 3 means we run out at T-999 — enough for most repos' lifetime. If you hit T-999, add a digit (T-1000, no padding required after that).

Re-using deleted IDs is forbidden — they may be referenced from old plans or handovers. Mark as `status: canceled` instead of deleting the file.
"#;

const PLANS_README: &str = r#"# `.sentinel/plans/`

Plans authored via Claude Code's `/plan` slash command that should live with the code.

## Why repo-local

Today `/plan` writes to `~/.claude/plans/<project>/<slug>-vN.md` (per-machine archive). That works for cross-session memory but fails for the case where the plan IS the spec for the next PR — the next developer (or a fresh clone) can't see it.

When a plan crosses the threshold from "scratchpad" to "design doc," move it here. `plan_organizer` hook (M9.2, not yet built) will eventually write plans here automatically based on a `commit-with-plan` marker; until then it's a manual `cp` from the global archive.

## Naming

`<slug>.md` — the slug is whatever the original plan file used. Versions stay in the global archive; here we keep the canonical version that matches the code.

## What goes here vs. in PR description

| `.sentinel/plans/<slug>.md` | PR description |
|-----------------------------|----------------|
| Pre-implementation design | Post-implementation summary |
| "What we're going to build and why" | "What changed and how to test it" |
| Edited only when design changes | Edited rarely after open |
| Refers forward (TBD code paths) | Refers backward (commits, files) |

Plans answer "why," PRs answer "what." Both live in version control once the work matures.
"#;

const HANDOVERS_README: &str = r#"# `.sentinel/handovers/`

Per-session summaries written when a session ends with non-trivial state to carry forward.

## When to write a handover

Not every session needs one. Write a handover when:

1. Work is in-progress and the next session (you tomorrow, or a teammate) needs context to resume.
2. A decision was made that future sessions should know about but isn't documented elsewhere (not in CONTRIBUTING.md, not in a memory file, not in a ticket).
3. Investigation produced findings worth preserving but no immediate action.

Don't write a handover when:

- The session shipped a clean PR — that PR's description IS the handover.
- The session was purely answering a question with no state change.
- The work is trivially resumable from the git state alone.

## Naming

`YYYY-MM-DD-<short-slug>.md` — e.g. `2026-05-14-doppler-personal-branch.md`. Date prefix sorts naturally, slug names the topic.

## Schema

Free-form Markdown. Suggested sections (use what's relevant, omit what isn't):

```markdown
# <title>

## Context
<what was the session trying to do>

## What got done
<concrete shipped artifacts: commits, PRs, files touched>

## What's still open
<tasks, decisions, follow-ups>

## Gotchas for next session
<surprises that wasted time, things that look fine but aren't>

## Pointers
<files, commits, task IDs, docs>
```
"#;

const LESSONS_README: &str = r#"# `.sentinel/lessons/`

Project-specific lessons the agent (or a human) learned about this codebase, persisted for future sessions.

## What goes here

Lessons that are:

- **Repo-specific** — not portable to other projects. Cross-project knowledge lives in global memory at `~/.claude/projects/<project>/memory/` instead.
- **Non-obvious** — can't be re-derived by reading the code in 5 minutes. Tribal knowledge that a fresh reader would miss.
- **Actionable** — affects how to do future work, not just trivia.

Examples that fit:

- "This crate's tests must be run with `--features integration` or they silently no-op."
- "The Auth0 dev tenant rate-limits at 30 req/min — burst tests need a backoff or they'll lock the test user."
- "Cargo.lock is committed but Cargo.toml uses workspace-relative paths that don't resolve in `.claude/worktrees/` — see `linear-mcp-rust/Cargo.toml:14` for the note."

Examples that don't fit (and where they belong):

- "Use `RUST_LOG=debug` for verbose logs" → CONTRIBUTING.md or README.
- "Slack thinks pizza is a vegetable" → Slack.
- "Gary prefers terse PR descriptions" → `~/.claude/projects/<project>/memory/` (global, applies across repos).

## Schema

JSON, one lesson per file, named `L-<NNN>.json`:

```json
{
  "id": "L-001",
  "title": "Cargo.toml worktree path issue",
  "summary": "...",
  "details": "Multi-paragraph explanation...",
  "tags": ["build", "worktrees", "cargo"],
  "first_observed": "2026-05-14",
  "still_valid_as_of": "2026-05-14",
  "related_commits": ["abc1234"],
  "related_files": ["linear-mcp-rust/Cargo.toml"]
}
```

`still_valid_as_of` is the date last verified. Lessons rot — when the underlying problem is fixed, mark the lesson resolved (don't delete; future sessions may search for the old symptom):

```json
{
  ...
  "resolved": true,
  "resolved_at": "2026-09-15",
  "resolution": "fix landed in commit def5678; cargo workspace paths now resolve in worktrees."
}
```

## Naming

`L-001`, `L-002`, ... — same padding-and-monotonic convention as tickets.
"#;

const PROOF_CHAINS_README: &str = r#"# `.sentinel/proof-chains/`

Repo-local cryptographic execution chains. Each session's chain (or an excerpted summary) lives here when it matters enough to ship with the code.

## Relationship to `~/.claude/sentinel/proofs/`

The global archive at `~/.claude/sentinel/proofs/<session_id>.json` (#73, commit e969b61) captures **every** session's full chain. That's a working dataset for the router-as-planner (M7.5 / #54) to learn from. Most sessions' chains do NOT need to be ship-with-code artifacts — they're per-machine analytics.

Repo-local proof-chains here are the exception: chains that *attest to a specific commit's correctness*. The proof chain that drove the work for commit `abc1234` lives at `.sentinel/proof-chains/abc1234.json` (or `<session_id>.json` referenced from the commit message). Reviewers, auditors, and future maintainers can verify the chain against the code without per-machine sentinel state.

## When to commit a chain here

Three triggers:

1. **Audit-grade trust tier** (M3.3 / #69 Stage B). A step's trust tier was set to `audit-grade` in its config — that's the signal that the chain should outlive the per-machine cache.
2. **High-risk change.** Production deploys, breaking schema changes, anything where "show me the chain" is the right answer to a future incident review.
3. **Explicit author opt-in.** The PR author decided this PR's chain is worth shipping. Commit message footer includes `Sentinel-Chain: .sentinel/proof-chains/<filename>.json`.

Don't commit every chain — proof chains are large (~10-50 KB each), and the per-machine archive at `~/.claude/sentinel/proofs/` handles the analytics use case. The repo-local copy is for the small subset that need to be reviewable against code.

## Naming

`<session_id>.json` matching the per-machine archive's filename. The chain itself carries the session's full identity (chain head hash, judge models, account context), so renaming the file is fine — the cryptographic identity is the contents, not the path.

## Verification

```bash
sentinel verify --session <session_id> --chain .sentinel/proof-chains/<filename>.json
```

(`sentinel verify` already exists for the global archive; the `--chain` flag to point at an arbitrary file is a small extension — TBD if not already there.)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_creates_all_subdirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        run(Some(tmp.path().to_path_buf()), false, false).unwrap();
        let sd = tmp.path().join(".sentinel");
        assert!(sd.join("README.md").is_file());
        assert!(sd.join("config.json").is_file());
        for (sub, _) in SUBDIRS {
            assert!(sd.join(sub).is_dir(), "missing dir: {sub}");
            assert!(sd.join(sub).join("README.md").is_file(), "missing README: {sub}");
        }
    }

    #[test]
    fn run_is_idempotent_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        run(Some(tmp.path().to_path_buf()), false, false).unwrap();
        // Modify a file so we can detect overwrite.
        let readme = tmp.path().join(".sentinel").join("README.md");
        std::fs::write(&readme, "MODIFIED").unwrap();
        // Re-run without force — file should be preserved.
        run(Some(tmp.path().to_path_buf()), false, false).unwrap();
        let after = std::fs::read_to_string(&readme).unwrap();
        assert_eq!(after, "MODIFIED", "non-force should NOT overwrite existing files");
    }

    #[test]
    fn run_with_force_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        run(Some(tmp.path().to_path_buf()), false, false).unwrap();
        let readme = tmp.path().join(".sentinel").join("README.md");
        std::fs::write(&readme, "MODIFIED").unwrap();
        run(Some(tmp.path().to_path_buf()), true, false).unwrap();
        let after = std::fs::read_to_string(&readme).unwrap();
        assert!(after.contains("Repo-local sentinel state"), "force should overwrite");
    }

    #[test]
    fn dry_run_does_not_create_anything() {
        let tmp = tempfile::tempdir().unwrap();
        run(Some(tmp.path().to_path_buf()), false, true).unwrap();
        assert!(!tmp.path().join(".sentinel").exists(), "dry-run must not create .sentinel/");
    }

    #[test]
    fn config_json_is_valid_json() {
        // The default config string must parse — catches typos at compile-bake time.
        let v: serde_json::Value = serde_json::from_str(DEFAULT_CONFIG_JSON).expect("valid JSON");
        assert!(v.is_object());
        assert_eq!(v.get("version").and_then(|x| x.as_i64()), Some(1));
    }

    #[test]
    fn second_run_after_first_creates_only_missing() {
        let tmp = tempfile::tempdir().unwrap();
        run(Some(tmp.path().to_path_buf()), false, false).unwrap();
        // Delete one subdir's README — simulates partial state.
        let lost = tmp.path().join(".sentinel").join("lessons").join("README.md");
        std::fs::remove_file(&lost).unwrap();
        // Re-run; the missing README should be re-created, others left alone.
        let readme = tmp.path().join(".sentinel").join("README.md");
        std::fs::write(&readme, "MODIFIED").unwrap();
        run(Some(tmp.path().to_path_buf()), false, false).unwrap();
        assert!(lost.is_file(), "deleted file should be re-created");
        let after_root = std::fs::read_to_string(&readme).unwrap();
        assert_eq!(after_root, "MODIFIED", "unrelated existing files preserved");
    }
}
