//! `sentinel project` subcommands.
//!
//! - `sentinel project init` (M9.1, #65): scaffold `.sentinel/` in a repo.
//! - `sentinel project handover` (M9.3, #67): append a handover Markdown
//!   stub to `.sentinel/handovers/YYYY-MM-DD-<slug>.md`.
//! - `sentinel project lesson` (M9.3, #67): append a lesson JSON file to
//!   `.sentinel/lessons/L-NNN.json` with the next monotonic ID.
//!
//! ---
//!
//! ## `sentinel project init` (M9.1, task #65)
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
                    p.strip_prefix(root).map_or_else(|_| p.display().to_string(), |s| s.display().to_string())
                );
            }
        }
        if !self.overwrote.is_empty() {
            let verb = if dry_run { "overwrite" } else { "Overwrote" };
            println!("  {verb} (--force):");
            for p in &self.overwrote {
                println!(
                    "    {}",
                    p.strip_prefix(root).map_or_else(|_| p.display().to_string(), |s| s.display().to_string())
                );
            }
        }
        if !self.skipped_existing.is_empty() {
            let verb = if dry_run { "skip" } else { "Skipped" };
            println!("  {verb} (already present, pass --force to overwrite):");
            for p in &self.skipped_existing {
                println!(
                    "    {}",
                    p.strip_prefix(root).map_or_else(|_| p.display().to_string(), |s| s.display().to_string())
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

const HANDOVERS_README: &str = r"# `.sentinel/handovers/`

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
";

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

// ============================================================================
// `sentinel project handover` — write a Markdown handover stub (M9.3, #67)
// ============================================================================

/// Write a handover stub to `<repo>/.sentinel/handovers/YYYY-MM-DD-<slug>.md`.
///
/// `title` is required — it becomes both the document's heading and the
/// filename slug (lowercased, non-alnum replaced with `-`). `summary` is
/// optional — when provided, it pre-fills the *Context* section so the
/// user only has to fill in the post-context sections. `dir` defaults to
/// the current working directory; the function walks up looking for the
/// repo root (presence of `.git`) and writes under that.
///
/// Refuses to overwrite an existing file at the same path — appends a `-2`
/// / `-3` / etc. suffix when collisions occur. This matters because two
/// handovers in the same day with the same slug shouldn't silently merge.
pub fn run_handover(
    dir: Option<PathBuf>,
    title: String,
    summary: Option<String>,
) -> Result<()> {
    if title.trim().is_empty() {
        anyhow::bail!("--title is required and cannot be empty");
    }
    let cwd = match dir {
        Some(d) => d,
        None => std::env::current_dir().context("could not resolve current directory")?,
    };
    let repo_root = find_repo_root(&cwd)
        .context("could not find a .git directory walking up from cwd — handovers require a git repo")?;
    let handovers_dir = repo_root.join(".sentinel").join("handovers");
    if !handovers_dir.is_dir() {
        anyhow::bail!(
            "{} does not exist — run `sentinel project init` first to scaffold .sentinel/",
            handovers_dir.display()
        );
    }

    let date = current_date_string();
    let slug = slugify(&title);
    let target = next_available_handover_path(&handovers_dir, &date, &slug);

    let body = render_handover_stub(&title, summary.as_deref(), &date);
    std::fs::write(&target, body).with_context(|| format!("write {}", target.display()))?;

    println!("Created handover: {}", target.display());
    Ok(())
}

/// Walk up from `cwd` looking for a `.git` entry. Returns the directory
/// containing `.git` (the repo root) on first match, or None.
fn find_repo_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Today's date as `YYYY-MM-DD` in the local timezone. The dependency
/// chain (chrono is already a workspace dep for the time-of-day work in
/// `session_init`) means no new deps for the date format.
fn current_date_string() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Slugify a free-form title into a filename-safe segment. Lowercase
/// alphanumerics pass through; everything else becomes `-`; runs of `-`
/// collapse; leading/trailing `-` trim.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        // All-non-alphanumeric title (emoji, punctuation) — fall back so
        // we always produce a usable filename.
        return "handover".to_string();
    }
    out
}

/// Resolve a non-colliding `YYYY-MM-DD-<slug>.md` path inside
/// `handovers_dir`. First try just the base name; on conflict, append
/// `-2`, `-3`, ... until we find a free slot.
fn next_available_handover_path(handovers_dir: &Path, date: &str, slug: &str) -> PathBuf {
    let base = handovers_dir.join(format!("{date}-{slug}.md"));
    if !base.exists() {
        return base;
    }
    for n in 2..1000 {
        let candidate = handovers_dir.join(format!("{date}-{slug}-{n}.md"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Should never hit in practice — 1000+ handovers with the same title
    // on the same day is operator error.
    handovers_dir.join(format!("{date}-{slug}-overflow.md"))
}

fn render_handover_stub(title: &str, summary: Option<&str>, date: &str) -> String {
    let summary_block = match summary {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "<what was the session trying to do — fill in>".to_string(),
    };
    format!(
        "# {title}\n\
         \n\
         **Date:** {date}\n\
         \n\
         ## Context\n\
         \n\
         {summary_block}\n\
         \n\
         ## What got done\n\
         \n\
         <concrete shipped artifacts: commits, PRs, files touched>\n\
         \n\
         ## What's still open\n\
         \n\
         <tasks, decisions, follow-ups>\n\
         \n\
         ## Gotchas for next session\n\
         \n\
         <surprises that wasted time, things that look fine but aren't>\n\
         \n\
         ## Pointers\n\
         \n\
         <files, commits, task IDs, docs>\n"
    )
}

// ============================================================================
// `sentinel project lesson` — write a lesson JSON (M9.3, #67)
// ============================================================================

/// Write a lesson record to `<repo>/.sentinel/lessons/L-<NNN>.json` with
/// the next monotonic ID. Uses the README's schema: `id`, `title`,
/// `summary`, `details`, `tags`, `first_observed`, `still_valid_as_of`,
/// `related_commits`, `related_files`, plus the resolved-marker fields
/// that are populated only when a lesson is closed out (omitted here so
/// the record starts in the active state).
pub fn run_lesson(
    dir: Option<PathBuf>,
    title: String,
    summary: Option<String>,
    tags: Vec<String>,
) -> Result<()> {
    if title.trim().is_empty() {
        anyhow::bail!("--title is required and cannot be empty");
    }
    let cwd = match dir {
        Some(d) => d,
        None => std::env::current_dir().context("could not resolve current directory")?,
    };
    let repo_root = find_repo_root(&cwd)
        .context("could not find a .git directory walking up from cwd — lessons require a git repo")?;
    let lessons_dir = repo_root.join(".sentinel").join("lessons");
    if !lessons_dir.is_dir() {
        anyhow::bail!(
            "{} does not exist — run `sentinel project init` first to scaffold .sentinel/",
            lessons_dir.display()
        );
    }

    let next_id = next_lesson_id(&lessons_dir)?;
    let target = lessons_dir.join(format!("L-{next_id:03}.json"));
    let body = render_lesson_json(&next_id_string(next_id), &title, summary.as_deref(), &tags);
    std::fs::write(&target, body).with_context(|| format!("write {}", target.display()))?;

    println!("Created lesson: {}", target.display());
    Ok(())
}

fn next_id_string(n: u32) -> String {
    format!("L-{n:03}")
}

/// Scan `lessons_dir` for `L-<NNN>.json` filenames and return the next
/// unused integer. Lessons must NOT reuse retired IDs — the README's
/// "mark resolved, don't delete" convention guarantees old IDs stay
/// occupied. We respect that by always returning max(existing) + 1.
fn next_lesson_id(lessons_dir: &Path) -> Result<u32> {
    let mut max_seen = 0u32;
    let entries = std::fs::read_dir(lessons_dir)
        .with_context(|| format!("read_dir({})", lessons_dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(num_str) = stem.strip_prefix("L-") else {
            continue;
        };
        if let Ok(n) = num_str.parse::<u32>() {
            if n > max_seen {
                max_seen = n;
            }
        }
    }
    Ok(max_seen + 1)
}

fn render_lesson_json(
    id: &str,
    title: &str,
    summary: Option<&str>,
    tags: &[String],
) -> String {
    let date = current_date_string();
    let summary_str = summary.unwrap_or("").trim();
    let tags_arr = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string());
    // Use serde_json::to_string_pretty on a Map to keep the field order
    // predictable + match the README convention. Manual JSON would diverge
    // on escaping; we go through serde so titles with quotes don't break.
    let mut map = serde_json::Map::new();
    map.insert("id".into(), serde_json::Value::String(id.to_string()));
    map.insert("title".into(), serde_json::Value::String(title.to_string()));
    map.insert("summary".into(), serde_json::Value::String(summary_str.to_string()));
    map.insert("details".into(), serde_json::Value::String(String::new()));
    map.insert(
        "tags".into(),
        serde_json::from_str(&tags_arr).unwrap_or(serde_json::Value::Array(vec![])),
    );
    map.insert("first_observed".into(), serde_json::Value::String(date.clone()));
    map.insert("still_valid_as_of".into(), serde_json::Value::String(date));
    map.insert("related_commits".into(), serde_json::Value::Array(vec![]));
    map.insert("related_files".into(), serde_json::Value::Array(vec![]));
    let mut out = serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .unwrap_or_default();
    out.push('\n');
    out
}

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

    // ─── handover tests ──────────────────────────────────────────

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Doppler personal branch"), "doppler-personal-branch");
        assert_eq!(slugify("FPCRM-123: fix login"), "fpcrm-123-fix-login");
        assert_eq!(slugify("trailing spaces   "), "trailing-spaces");
        assert_eq!(slugify("  leading"), "leading");
        assert_eq!(slugify("!!!"), "handover");
        assert_eq!(slugify(""), "handover");
    }

    #[test]
    fn next_available_handover_path_picks_base_when_free() {
        let tmp = tempfile::tempdir().unwrap();
        let p = next_available_handover_path(tmp.path(), "2026-05-14", "test");
        assert_eq!(p, tmp.path().join("2026-05-14-test.md"));
    }

    #[test]
    fn next_available_handover_path_increments_on_collision() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("2026-05-14-test.md"), "").unwrap();
        std::fs::write(tmp.path().join("2026-05-14-test-2.md"), "").unwrap();
        let p = next_available_handover_path(tmp.path(), "2026-05-14", "test");
        assert_eq!(p, tmp.path().join("2026-05-14-test-3.md"));
    }

    #[test]
    fn run_handover_writes_to_correct_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".sentinel").join("handovers")).unwrap();

        run_handover(Some(repo.clone()), "Test handover".to_string(), None).unwrap();
        let date = current_date_string();
        let expected = repo.join(".sentinel").join("handovers").join(format!("{date}-test-handover.md"));
        assert!(expected.is_file(), "expected {} to exist", expected.display());
        let body = std::fs::read_to_string(&expected).unwrap();
        assert!(body.starts_with("# Test handover"));
        assert!(body.contains(&format!("**Date:** {date}")));
        assert!(body.contains("## Context"));
    }

    #[test]
    fn run_handover_with_summary_fills_context() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".sentinel").join("handovers")).unwrap();

        run_handover(
            Some(repo.clone()),
            "T".to_string(),
            Some("summary text here".to_string()),
        )
        .unwrap();
        let date = current_date_string();
        let path = repo.join(".sentinel").join("handovers").join(format!("{date}-t.md"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("summary text here"));
        assert!(!body.contains("<what was the session trying to do"));
    }

    #[test]
    fn run_handover_errors_when_not_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        // No .sentinel/handovers/ — should fail clearly.
        let err = run_handover(Some(repo), "T".to_string(), None).unwrap_err();
        assert!(err.to_string().contains("sentinel project init"));
    }

    #[test]
    fn run_handover_errors_when_not_in_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // No .git anywhere.
        let err = run_handover(Some(tmp.path().to_path_buf()), "T".to_string(), None).unwrap_err();
        assert!(err.to_string().contains(".git"));
    }

    #[test]
    fn run_handover_errors_on_empty_title() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_handover(Some(tmp.path().to_path_buf()), "   ".to_string(), None).unwrap_err();
        assert!(err.to_string().contains("title"));
    }

    // ─── lesson tests ────────────────────────────────────────────

    #[test]
    fn next_lesson_id_starts_at_1() {
        let tmp = tempfile::tempdir().unwrap();
        let n = next_lesson_id(tmp.path()).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn next_lesson_id_finds_max_plus_one() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("L-001.json"), "").unwrap();
        std::fs::write(tmp.path().join("L-007.json"), "").unwrap();
        std::fs::write(tmp.path().join("L-003.json"), "").unwrap();
        // Junk files don't affect counting.
        std::fs::write(tmp.path().join("notes.txt"), "").unwrap();
        std::fs::write(tmp.path().join("L-abc.json"), "").unwrap();
        let n = next_lesson_id(tmp.path()).unwrap();
        assert_eq!(n, 8);
    }

    #[test]
    fn run_lesson_creates_json_with_monotonic_id() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join(".sentinel").join("lessons")).unwrap();

        run_lesson(
            Some(repo.clone()),
            "First lesson".to_string(),
            Some("did a thing".to_string()),
            vec!["build".to_string(), "windows".to_string()],
        )
        .unwrap();
        let path = repo.join(".sentinel").join("lessons").join("L-001.json");
        assert!(path.is_file());
        let raw = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["id"], "L-001");
        assert_eq!(v["title"], "First lesson");
        assert_eq!(v["summary"], "did a thing");
        assert_eq!(v["tags"], serde_json::json!(["build", "windows"]));
        assert!(v["first_observed"].is_string());
        assert!(v["still_valid_as_of"].is_string());
        assert_eq!(v["related_commits"], serde_json::json!([]));
        assert_eq!(v["related_files"], serde_json::json!([]));
    }

    #[test]
    fn run_lesson_assigns_next_id_after_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let lessons = repo.join(".sentinel").join("lessons");
        std::fs::create_dir_all(&lessons).unwrap();
        std::fs::write(lessons.join("L-042.json"), "{}").unwrap();

        run_lesson(Some(repo.clone()), "T".to_string(), None, vec![]).unwrap();
        assert!(lessons.join("L-043.json").is_file());
    }

    #[test]
    fn run_lesson_errors_when_not_initialized() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let err = run_lesson(Some(repo), "T".to_string(), None, vec![]).unwrap_err();
        assert!(err.to_string().contains("sentinel project init"));
    }

    #[test]
    fn run_lesson_errors_on_empty_title() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_lesson(Some(tmp.path().to_path_buf()), "".to_string(), None, vec![]).unwrap_err();
        assert!(err.to_string().contains("title"));
    }
}
