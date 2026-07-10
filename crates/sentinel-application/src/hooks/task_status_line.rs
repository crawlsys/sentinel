//! Task Status Line Hook — tasks as first-class citizens.
//!
//! Runs on **every** `UserPromptSubmit`. Reads the live native TaskList for the
//! current session (`~/.claude/tasks/{session_id}/` or the `session-{group}`
//! variant, resolved by [`super::session_task_dir`]) and injects an emoji task
//! block so the operator's in-flight work is visible on every turn:
//!
//! ```text
//! 📋 [Tasks] sentinel — 2 pending, 1 in progress, 8 done
//!   🔄🟠 #34 Make tasks first-class: every-turn status line (2/5)
//!   ⏳🔴🚫 #35 Wire the next thing →blocks 1
//! ```
//!
//! When the operator is working inside a **Linear-backed project**, a second
//! block is stacked below with the active issues of the current initiative,
//! read from the on-disk cache `~/.claude/sentinel/linear-assigned.json` (the
//! same file `linear_pm_gate` consumes — never a live API call on the hot
//! path). "Active" is the complement of Linear's terminal states, so anything
//! not `completed`/`canceled` shows:
//!
//! ```text
//! 📌 [Linear] Firefly Pro Beta — 6 active (3 in progress)
//!   🔄 FPCRM-607 contacts, migrate huntsville customers… (5)
//!   🗄️ FPCRM-610 billing, reconcile legacy invoices…
//!   🔺 FPFIELD-132 Maps: replace public DEMO_MAP_ID…
//! ```
//!
//! The two blocks are independent: the Linear overlay can render with no native
//! tasks, and the task block renders outside any Linear project. When both are
//! empty the hook injects nothing.
//!
//! Each line's leading glyph run is composed from the task's REAL fields:
//! status colour, then `metadata.priority` colour (`🔴`P0 … `🟢`P3), then a `🚫`
//! blocked marker when `blockedBy` is non-empty. Trailing hints show
//! `metadata.checklist` progress `(done/total)` and a `→blocks N` fan-out. The
//! subject is stripped of any baked-in decoration first, so a glyph the on-disk
//! decorator (`task_persist`) already applied is never doubled.
//!
//! Unlike `todo_loader` (which reads the `TodoWrite` store `active.jsonl` and
//! loads once per session), this hook reads the durable **TaskList** the
//! operator's CLAUDE.md mandates, and fires **every turn** — the queue is a
//! first-class citizen, not a one-shot banner.
//!
//! Design guarantees:
//! - **Session-scoped**: only THIS session's task dir is read, so the output is
//!   small and relevant (no cross-session graveyard — the `todo_loader` 4451
//!   lesson).
//! - **Silent when empty**: no active tasks → inject nothing (never spam an
//!   empty line every turn).
//! - **Robust**: unreadable dir / malformed files → silent; never panics;
//!   subject truncation is char-boundary-safe.
//! - **Canonical emoji**: status glyphs come from
//!   [`sentinel_domain::task_decoration::status_glyph`] — the single source of
//!   truth shared with the CLAUDE.md Active Tasks table.
//!
//! All IO goes through `ctx.fs` (`FileSystemPort`).

use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::task_decoration::{priority_glyph, status_glyph, strip_decoration};

use super::{concrete_input_session_id, FileSystemPort, HookContext};

/// Max active tasks rendered as individual lines; the rest are summarised as
/// `+N more`. Keeps a very long queue from dominating every message.
const MAX_LISTED: usize = 10;

/// Subject text is truncated to this many CHARACTERS (not bytes — multibyte
/// safe) so one verbose task can't blow out the line width.
const MAX_SUBJECT_CHARS: usize = 80;

/// A single native TaskList entry. Only the fields this line needs, so it
/// deserializes any TaskList row shape. Priority + checklist live in the
/// free-form `metadata` record — the native Task schema (recovered from
/// decompiled 2.1.206) carries neither as a first-class field.
#[derive(Debug, serde::Deserialize)]
struct TaskRow {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    status: String,
    /// Task IDs blocking this one — non-empty means "blocked" (there is no
    /// blocked status variant, so this is the sole blocked signal).
    #[serde(default, rename = "blockedBy")]
    blocked_by: Vec<String>,
    /// Task IDs this one blocks (rendered as a `→N` fan-out hint).
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default)]
    metadata: TaskMetadata,
}

/// The subset of a task's `metadata` record the status line surfaces.
#[derive(Debug, Default, serde::Deserialize)]
struct TaskMetadata {
    /// Priority, as a string (`"P0"`, `"high"`, …) or a number (Linear 1..4).
    #[serde(default)]
    priority: Option<serde_json::Value>,
    /// Checklist progress, either a `"3/5"` string or a `{done, total}` object.
    #[serde(default)]
    checklist: Option<serde_json::Value>,
}

impl TaskRow {
    /// Priority as a display string (`metadata.priority` normalised to a `str`),
    /// or `None` when absent/unrecognised shape.
    fn priority_str(&self) -> Option<String> {
        match self.metadata.priority.as_ref()? {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// `(done, total)` checklist progress from `metadata.checklist`, accepting
    /// either `"3/5"` or `{"done":3,"total":5}`. `None` when absent/unparseable.
    fn checklist_progress(&self) -> Option<(u64, u64)> {
        match self.metadata.checklist.as_ref()? {
            serde_json::Value::String(s) => {
                let (d, t) = s.split_once('/')?;
                Some((d.trim().parse().ok()?, t.trim().parse().ok()?))
            }
            serde_json::Value::Object(o) => {
                let done = o.get("done").and_then(serde_json::Value::as_u64)?;
                let total = o.get("total").and_then(serde_json::Value::as_u64)?;
                Some((done, total))
            }
            _ => None,
        }
    }
}

// ===========================================================================
// Linear overlay (#37) — active initiative issues, rendered under the task
// block when the operator is working inside a Linear-backed project.
//
// The status line runs on the hot path (every UserPromptSubmit), so we NEVER
// hit the Linear API here. We read the on-disk cache the SessionStart key-cache
// + inbound-sync machinery already maintain — the SAME file `linear_pm_gate`
// consumes — and filter it to the active issues locally.
// ===========================================================================

/// Max active Linear issues rendered as individual lines before the rest are
/// summarised as `+N more`. Kept small so the overlay never dominates a turn.
const MAX_LINEAR_LISTED: usize = 8;

/// The `linear-assigned.json` cache the overlay reads, at
/// `<home>/.claude/sentinel/linear-assigned.json`. Only the fields the overlay
/// renders are modelled; unknown keys are ignored so the schema can grow.
#[derive(Debug, serde::Deserialize)]
struct LinearCache {
    /// Human name of the active initiative (e.g. `"Firefly Pro Beta"`), shown
    /// in the block header. Optional so a headerless cache still renders.
    #[serde(default)]
    initiative: Option<String>,
    #[serde(default)]
    issues: Vec<LinearIssue>,
}

/// One issue row from the cache. Priority/assignee are intentionally absent —
/// the cache does not carry them, so the overlay never fabricates a priority
/// colour (unlike the native-task lines, whose priority lives in `metadata`).
#[derive(Debug, serde::Deserialize)]
struct LinearIssue {
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    title: String,
    /// Story points, when sized. Rendered as a trailing `(N)` hint.
    #[serde(default)]
    estimate: Option<u64>,
    #[serde(default)]
    state: LinearState,
}

/// The workflow-state of an issue. `type` is Linear's canonical state category
/// (`triage`/`backlog`/`unstarted`/`started`/`completed`/`canceled`) — the
/// stable key the overlay filters + glyphs on. The workspace's display `name`
/// is deliberately not modelled: the overlay renders the category glyph, not
/// the (workspace-specific, noisier) label.
#[derive(Debug, Default, serde::Deserialize)]
struct LinearState {
    #[serde(default, rename = "type")]
    type_: String,
}

impl LinearIssue {
    /// Is this issue "active" for overlay purposes? Per the operator's choice
    /// (2026-07-10) the overlay shows as much in-flight context as possible:
    /// everything that is NOT finished. Linear's terminal categories are
    /// `completed` and `canceled`; every other category — `started`,
    /// `unstarted` (todo), `backlog`, `triage`, and any future/unknown
    /// non-terminal category — counts as active. Defining "active" as the
    /// complement of the two terminal states means a new category surfaces
    /// rather than silently vanishing.
    fn is_active(&self) -> bool {
        !matches!(self.state.type_.as_str(), "completed" | "canceled")
    }

    /// Leading glyph for the issue's state category: `🔄` in-progress,
    /// `⏳` queued/unstarted, `🗄️` backlog, `🔺` awaiting triage. Any other
    /// (non-terminal) category falls back to `•`.
    fn state_glyph(&self) -> &'static str {
        match self.state.type_.as_str() {
            "started" => "🔄",
            "unstarted" => "⏳",
            "backlog" => "🗄️",
            "triage" => "🔺",
            _ => "•",
        }
    }

    /// Sort rank so the overlay lists in-flight work first, then the queue,
    /// then backlog, then triage — mirroring the native task block's
    /// in_progress-first order. Unknown categories sort last.
    fn sort_rank(&self) -> u8 {
        match self.state.type_.as_str() {
            "started" => 0,
            "unstarted" => 1,
            "backlog" => 2,
            "triage" => 3,
            _ => 4,
        }
    }
}

/// Path to the Linear cache: `<home>/.claude/sentinel/linear-assigned.json`.
fn linear_cache_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(super::sentinel_dir(&home).join("linear-assigned.json"))
}

/// Read + parse the Linear cache, or `None` when it is absent/unreadable/
/// malformed. Never fatal — a broken cache simply hides the overlay.
fn read_linear_cache(fs: &dyn FileSystemPort) -> Option<LinearCache> {
    let path = linear_cache_path(fs)?;
    let content = fs.read_to_string(&path).ok()?;
    serde_json::from_str::<LinearCache>(&content).ok()
}

/// Does `cwd` sit inside a **Linear-backed** project? The overlay only fires
/// there (the operator's "when it uses Linear for a project" rule), so Firefly
/// issues never surface while working in an unrelated repo.
///
/// Reads the canonical project configs at `<home>/.claude/sentinel/projects/`
/// (NOT the legacy `~/.claude/projects/`, which holds only stragglers +
/// conversation transcripts). A project counts as Linear-backed when its
/// frontmatter declares any of `linear_workspace` / `linear_teams` /
/// `issue_prefix`, AND the cwd matches one of the project's identity tokens
/// (file stem, `name`, `aliases`). Returns the project's display name on match.
fn cwd_is_linear_backed_project(fs: &dyn FileSystemPort, cwd: &str) -> Option<String> {
    let home = fs.home_dir()?;
    let dir = super::sentinel_dir(&home).join("projects");
    let entries = fs.read_dir(&dir).ok()?;
    for path in entries {
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.eq_ignore_ascii_case("_template") || stem.eq_ignore_ascii_case("MEMORY") {
            continue;
        }
        let Ok(content) = fs.read_to_string(&path) else {
            continue;
        };
        let Some(fm) = extract_frontmatter(&content) else {
            continue;
        };
        if !frontmatter_is_linear_backed(fm) {
            continue;
        }
        let tokens = frontmatter_project_tokens(fm, stem);
        if cwd_matches_project_token(cwd, &tokens) {
            return Some(project_display_name(fm, stem));
        }
    }
    None
}

/// A project's frontmatter is Linear-backed when it declares a workspace, a
/// team list, or an issue prefix — any one is enough to mean "uses Linear".
fn frontmatter_is_linear_backed(fm: &str) -> bool {
    fm.lines().map(str::trim).any(|l| {
        l.starts_with("linear_workspace:")
            || l.starts_with("linear_teams:")
            || l.starts_with("linear_account:")
            || l.starts_with("issue_prefix:")
    })
}

/// The YAML frontmatter block between the leading `---` fences, if present.
fn extract_frontmatter(content: &str) -> Option<&str> {
    let trimmed = content.trim_start();
    let rest = trimmed.strip_prefix("---")?;
    let rest = rest.trim_start_matches('\n').trim_start_matches('\r');
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Identity tokens for a project: file stem, `name:`, and each `aliases:` entry,
/// lowercased and length-filtered (≥3) so a stray short alias can't match a
/// generic path segment. Mirrors `commit_message_validator`'s token logic.
fn frontmatter_project_tokens(fm: &str, stem: &str) -> Vec<String> {
    let mut tokens = vec![stem.to_lowercase()];
    for line in fm.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            tokens.push(unquote(rest).to_lowercase());
        } else if let Some(rest) = line.strip_prefix("aliases:") {
            let rest = rest.trim();
            if let Some(inner) = rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                for a in inner.split(',') {
                    let clean = unquote(a.trim()).to_lowercase();
                    if !clean.is_empty() {
                        tokens.push(clean);
                    }
                }
            }
        }
    }
    tokens.into_iter().filter(|t| t.len() >= 3).collect()
}

/// The project's human name (`name:` frontmatter, else the file stem).
fn project_display_name(fm: &str, stem: &str) -> String {
    for line in fm.lines() {
        if let Some(rest) = line.trim().strip_prefix("name:") {
            let n = unquote(rest);
            if !n.is_empty() {
                return n.to_string();
            }
        }
    }
    stem.to_string()
}

/// Strip surrounding quotes/whitespace from a YAML scalar.
fn unquote(s: &str) -> &str {
    s.trim().trim_matches('"').trim_matches('\'').trim()
}

/// Segment-exact cwd match (never substring) so token `"crm"` matches a
/// `.../crm/...` path segment but not `crm-archive`. Mirrors the commit
/// validator's `cwd_matches_tokens`.
fn cwd_matches_project_token(cwd: &str, tokens: &[String]) -> bool {
    let segments: Vec<String> = cwd
        .replace('\\', "/")
        .to_lowercase()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    tokens.iter().any(|t| segments.iter().any(|s| s == t))
}

/// Build the Linear overlay block from the cache, or `None` when there is
/// nothing active to show. Pure so it unit-tests directly. `project_name` is
/// the resolved Linear-backed project's display name (used only if the cache
/// has no `initiative` header).
fn render_linear_block(cache: &LinearCache, project_name: &str) -> Option<String> {
    let mut active: Vec<&LinearIssue> = cache.issues.iter().filter(|i| i.is_active()).collect();
    if active.is_empty() {
        return None;
    }
    active.sort_by_key(|i| i.sort_rank());

    let header_label = cache
        .initiative
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(project_name);

    use std::fmt::Write as _;
    let started = active.iter().filter(|i| i.state.type_ == "started").count();
    let mut block = format!(
        "📌 [Linear] {header_label} — {} active ({started} in progress)",
        active.len()
    );
    for i in active.iter().take(MAX_LINEAR_LISTED) {
        let mut hints = String::new();
        if let Some(est) = i.estimate {
            let _ = write!(hints, " ({est})");
        }
        let _ = write!(
            block,
            "\n  {} {} {}{hints}",
            i.state_glyph(),
            i.identifier,
            truncate_subject(i.title.trim()),
        );
    }
    let overflow = active.len().saturating_sub(MAX_LINEAR_LISTED);
    if overflow > 0 {
        let _ = write!(block, "\n  …and {overflow} more active");
    }
    Some(block)
}

/// Resolve this session's native TaskList directory (literal `{session_id}` or
/// the `session-{group}` variant), or `None` when the session id is absent.
fn task_dir(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(super::session_task_dir(fs, &home, session_id))
}

/// Read every `*.json` task file in `dir` (skipping dotfiles / non-json), sorted
/// by numeric id. Any read/parse failure is skipped, not fatal.
fn read_tasks(fs: &dyn FileSystemPort, dir: &Path) -> Vec<TaskRow> {
    let mut tasks = Vec::new();
    let Ok(entries) = fs.read_dir(dir) else {
        return tasks;
    };
    for path in entries {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.starts_with('.')
            || !Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        if let Ok(content) = fs.read_to_string(&path) {
            if let Ok(task) = serde_json::from_str::<TaskRow>(&content) {
                if !task.id.is_empty() {
                    tasks.push(task);
                }
            }
        }
    }
    tasks.sort_by(|a, b| {
        let a_num: u32 = a.id.parse().unwrap_or(u32::MAX);
        let b_num: u32 = b.id.parse().unwrap_or(u32::MAX);
        a_num.cmp(&b_num).then_with(|| a.id.cmp(&b.id))
    });
    tasks
}

fn is_active(status: &str) -> bool {
    status == "in_progress" || status == "pending"
}

/// Truncate a subject to `MAX_SUBJECT_CHARS` on a char boundary, appending `…`
/// when it was cut. `&s[..n]` would panic mid-multibyte — take chars instead.
fn truncate_subject(subject: &str) -> String {
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        format!(
            "{}…",
            subject.chars().take(MAX_SUBJECT_CHARS).collect::<String>()
        )
    } else {
        subject.to_string()
    }
}

/// Build the injected task block from the current task set, or `None` when
/// there is nothing active to show. Pure so it can be unit-tested directly.
fn render_block(tasks: &[TaskRow], project_name: &str) -> Option<String> {
    let pending = tasks.iter().filter(|t| t.status == "pending").count();
    let in_progress = tasks
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();
    let done = tasks.iter().filter(|t| t.status == "completed").count();

    // In-progress first (what you're doing now), then pending (what's next).
    let mut active: Vec<&TaskRow> = tasks.iter().filter(|t| is_active(&t.status)).collect();
    if active.is_empty() {
        return None;
    }
    active.sort_by_key(|t| match t.status.as_str() {
        "in_progress" => 0,
        _ => 1,
    });

    use std::fmt::Write as _;
    let mut block = format!(
        "📋 [Tasks] {project_name} — {pending} pending, {in_progress} in progress, {done} done"
    );
    for t in active.iter().take(MAX_LISTED) {
        // Build the leading glyph run from the task's REAL fields — status,
        // then priority colour, then a blocked marker. The subject is stripped
        // first so a glyph already baked into it on disk (by task_persist's
        // decorator) is not doubled: this was the "🔄 🔄 #38" bug.
        let mut glyphs = String::new();
        glyphs.push_str(status_glyph(&t.status).unwrap_or("•"));
        if let Some(g) = t.priority_str().as_deref().and_then(priority_glyph) {
            glyphs.push_str(g);
        }
        let blocked = !t.blocked_by.is_empty();
        if blocked && status_glyph(&t.status) != Some("🚫") {
            glyphs.push('🚫');
        }

        // Trailing hints: checklist progress and fan-out (how many this blocks).
        let mut hints = String::new();
        if let Some((done, total)) = t.checklist_progress() {
            let _ = write!(hints, " ({done}/{total})");
        }
        if !t.blocks.is_empty() {
            let _ = write!(hints, " →blocks {}", t.blocks.len());
        }

        let _ = write!(
            block,
            "\n  {glyphs} #{} {}{hints}",
            t.id,
            truncate_subject(strip_decoration(&t.subject))
        );
    }
    let overflow = active.len().saturating_sub(MAX_LISTED);
    if overflow > 0 {
        let _ = write!(block, "\n  …and {overflow} more active");
    }
    Some(block)
}

/// Process the task-status-line hook event. Fires every UserPromptSubmit.
///
/// Emits up to two stacked blocks:
///   1. `📋 [Tasks]` — this session's native TaskList (session-scoped).
///   2. `📌 [Linear]` — active initiative issues, but ONLY when the cwd is a
///      Linear-backed project (the overlay from #37).
///
/// The two are independent: the Linear block can show with no native tasks, and
/// the task block can show outside any Linear project. When both are empty the
/// hook injects nothing (never spam an empty line every turn).
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let cwd_project_name = Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace");

    // --- Task block (session-scoped native TaskList) ---------------------
    let task_block = concrete_input_session_id(input)
        .and_then(|session_id| task_dir(ctx.fs, session_id))
        .filter(|dir| ctx.fs.is_dir(dir))
        .and_then(|dir| render_block(&read_tasks(ctx.fs, &dir), cwd_project_name));

    // --- Linear overlay (only inside a Linear-backed project) ------------
    // The project gate is the cheap check; the cache read is the I/O, so it is
    // only reached when the gate passes (never read outside a Linear project).
    let linear_block = cwd_is_linear_backed_project(ctx.fs, cwd).and_then(|project| {
        read_linear_cache(ctx.fs).and_then(|cache| render_linear_block(&cache, &project))
    });

    match combine_blocks(task_block, linear_block) {
        Some(block) => HookOutput::inject_context(HookEvent::UserPromptSubmit, block),
        None => HookOutput::allow(),
    }
}

/// Stack the task and Linear blocks with a blank line between them, or return
/// whichever is present, or `None` when both are empty.
fn combine_blocks(task: Option<String>, linear: Option<String>) -> Option<String> {
    match (task, linear) {
        (Some(t), Some(l)) => Some(format!("{t}\n{l}")),
        (Some(t), None) => Some(t),
        (None, Some(l)) => Some(l),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    fn row(id: &str, status: &str, subject: &str) -> TaskRow {
        TaskRow {
            id: id.into(),
            status: status.into(),
            subject: subject.into(),
            blocked_by: Vec::new(),
            blocks: Vec::new(),
            metadata: TaskMetadata::default(),
        }
    }

    /// Deserialize a `TaskRow` from a raw JSON object literal — exercises the
    /// real `metadata`/`blockedBy`/`blocks` parsing the on-disk files use.
    fn row_json(v: serde_json::Value) -> TaskRow {
        serde_json::from_value(v).expect("valid TaskRow json")
    }

    #[test]
    fn render_lists_active_tasks_with_canonical_glyphs() {
        let tasks = vec![
            row("1", "completed", "old done thing"),
            row("2", "pending", "next up"),
            row("33", "in_progress", "the current work"),
        ];
        let block = render_block(&tasks, "sentinel").expect("active tasks → block");
        // Summary counts.
        assert!(
            block.contains("1 pending, 1 in progress, 1 done"),
            "{block}"
        );
        // In-progress rendered first, with the canonical 🔄 glyph + id + subject.
        assert!(block.contains("🔄 #33 the current work"), "{block}");
        assert!(block.contains("⏳ #2 next up"), "{block}");
        // Completed tasks are counted but not listed as active lines.
        assert!(!block.contains("old done thing"), "{block}");
        // Ordering: the in_progress line precedes the pending line.
        let ip = block.find("🔄 #33").unwrap();
        let pd = block.find("⏳ #2").unwrap();
        assert!(ip < pd, "in_progress must be listed before pending: {block}");
    }

    #[test]
    fn empty_or_all_done_renders_nothing() {
        // No tasks at all.
        assert!(render_block(&[], "sentinel").is_none());
        // Only completed → nothing active to show.
        let done = vec![row("1", "completed", "done")];
        assert!(
            render_block(&done, "sentinel").is_none(),
            "all-done queue must stay silent"
        );
    }

    #[test]
    fn subject_truncation_is_char_boundary_safe() {
        // A multibyte subject longer than the cap must not panic and must be
        // truncated with an ellipsis. Box-drawing chars are 3 bytes each.
        let long = "│".repeat(200);
        let tasks = vec![row("1", "in_progress", &long)];
        let block = render_block(&tasks, "p").expect("block");
        assert!(block.contains('…'), "long subject should be ellipsised");
        // The rendered subject must be exactly MAX_SUBJECT_CHARS chars + '…'.
        let line = block.lines().find(|l| l.contains("#1")).unwrap();
        let subj_chars = line.chars().filter(|&c| c == '│').count();
        assert_eq!(subj_chars, MAX_SUBJECT_CHARS, "truncated to the char cap");
    }

    #[test]
    fn overflow_summarised_after_cap() {
        let tasks: Vec<TaskRow> = (1..=15)
            .map(|i| row(&i.to_string(), "pending", &format!("task {i}")))
            .collect();
        let block = render_block(&tasks, "p").expect("block");
        let listed = block.matches("⏳ #").count();
        assert_eq!(listed, MAX_LISTED, "only the cap is listed individually");
        assert!(block.contains("…and 5 more active"), "{block}");
    }

    #[test]
    fn process_fires_every_turn_no_one_shot_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        // Seed a native task dir for the session with one in_progress task.
        let sid = "tsl-sess-1";
        let dir = tmp.path().join(".claude").join("tasks").join(sid);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"live task","status":"in_progress"}"#,
        )
        .unwrap();

        let input = HookInput {
            session_id: Some(sid.to_string()),
            cwd: Some("/repo/sentinel".to_string()),
            ..Default::default()
        };

        // Fire twice — both must inject (no one-shot suppression).
        for _ in 0..2 {
            let out = process(&input, &ctx);
            let ctx_str = out
                .hook_specific_output
                .and_then(|h| h.additional_context)
                .expect("task line must inject every turn");
            assert!(ctx_str.contains("📋 [Tasks] sentinel"), "{ctx_str}");
            assert!(ctx_str.contains("🔄 #1 live task"), "{ctx_str}");
        }
    }

    #[test]
    fn process_silent_when_no_task_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let input = HookInput {
            session_id: Some("tsl-no-dir".to_string()),
            cwd: Some("/repo/x".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(
            out.hook_specific_output.is_none(),
            "a session with no task dir must inject nothing"
        );
    }

    #[test]
    fn process_reads_session_prefixed_dir_variant() {
        // Claude Code sometimes writes the native dir as `session-{group}`
        // rather than the full session id. session_task_dir resolves both;
        // confirm the line reads the prefixed variant.
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let sid = "abcd1234-ef00-0000-0000-000000000000";
        let prefixed = tmp
            .path()
            .join(".claude")
            .join("tasks")
            .join("session-abcd1234");
        std::fs::create_dir_all(&prefixed).unwrap();
        std::fs::write(
            prefixed.join("7.json"),
            r#"{"id":"7","subject":"prefixed dir task","status":"pending"}"#,
        )
        .unwrap();

        let input = HookInput {
            session_id: Some(sid.to_string()),
            cwd: Some("/r/proj".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        let ctx_str = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("must read the session-prefixed dir");
        assert!(ctx_str.contains("⏳ #7 prefixed dir task"), "{ctx_str}");
    }

    #[test]
    fn process_returns_allow_on_missing_session() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let out = process(&input, &ctx);
        assert!(out.blocked.is_none());
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn already_decorated_subject_is_not_doubled() {
        // The bug: task_persist bakes "🔄 Fix it" into the on-disk subject, and
        // the status line used to prefix its own 🔄 → "🔄 🔄". The line must
        // strip first, so exactly one status glyph appears.
        let tasks = vec![row("38", "in_progress", "🔄 Fix the gate")];
        let block = render_block(&tasks, "sentinel").expect("block");
        assert!(block.contains("🔄 #38 Fix the gate"), "{block}");
        assert!(!block.contains("🔄 🔄"), "no doubled glyph: {block}");
        // And the baked subject's glyph doesn't leak into the text.
        assert!(!block.contains("#38 🔄"), "{block}");
    }

    #[test]
    fn priority_colour_from_metadata_is_rendered() {
        let t = row_json(serde_json::json!({
            "id": "5", "status": "in_progress", "subject": "urgent work",
            "metadata": {"priority": "P0"}
        }));
        let block = render_block(&[t], "p").expect("block");
        // status 🔄 then priority 🔴, then id + subject.
        assert!(block.contains("🔄🔴 #5 urgent work"), "{block}");
    }

    #[test]
    fn blocked_marker_from_blocked_by() {
        let t = row_json(serde_json::json!({
            "id": "6", "status": "pending", "subject": "waiting",
            "blockedBy": ["3", "4"]
        }));
        let block = render_block(&[t], "p").expect("block");
        assert!(block.contains("⏳🚫 #6 waiting"), "{block}");
    }

    #[test]
    fn checklist_and_blocks_hints() {
        let t = row_json(serde_json::json!({
            "id": "7", "status": "in_progress", "subject": "big task",
            "blocks": ["8", "9"],
            "metadata": {"checklist": "3/5"}
        }));
        let block = render_block(&[t], "p").expect("block");
        assert!(block.contains("#7 big task (3/5) →blocks 2"), "{block}");

        // Object-shaped checklist works too.
        let t2 = row_json(serde_json::json!({
            "id": "8", "status": "pending", "subject": "obj checklist",
            "metadata": {"checklist": {"done": 1, "total": 4}}
        }));
        let block2 = render_block(&[t2], "p").expect("block");
        assert!(block2.contains("#8 obj checklist (1/4)"), "{block2}");
    }

    #[test]
    fn numeric_linear_priority_maps_to_colour() {
        // Linear stores priority as 1..4; metadata may carry the raw number.
        let t = row_json(serde_json::json!({
            "id": "9", "status": "pending", "subject": "num pri",
            "metadata": {"priority": 2}
        }));
        let block = render_block(&[t], "p").expect("block");
        // 2 → high → 🟠.
        assert!(block.contains("⏳🟠 #9 num pri"), "{block}");
    }

    // =======================================================================
    // Linear overlay (#37)
    // =======================================================================

    /// Build a `LinearCache` from a raw JSON literal — exercises the real
    /// serde path (unknown-key tolerance, `state.type` rename, optional fields).
    fn cache_json(v: serde_json::Value) -> LinearCache {
        serde_json::from_value(v).expect("valid LinearCache json")
    }

    fn issue(identifier: &str, state_type: &str, title: &str) -> serde_json::Value {
        serde_json::json!({
            "identifier": identifier,
            "title": title,
            "state": {"type": state_type, "name": state_type},
        })
    }

    #[test]
    fn linear_active_filter_is_complement_of_terminal_states() {
        // Per Gary's 2026-07-10 choice ("as much as possible"): everything that
        // is NOT completed/canceled is shown — started, todo, backlog, triage,
        // and any unknown non-terminal category. Only the two terminal states
        // are hidden.
        let cache = cache_json(serde_json::json!({
            "initiative": "Firefly Pro Beta",
            "issues": [
                issue("FP-1", "started",   "in flight"),
                issue("FP-2", "unstarted", "queued"),
                issue("FP-3", "backlog",   "later"),
                issue("FP-4", "triage",    "needs triage"),
                issue("FP-5", "future",    "unknown non-terminal — shown"),
                issue("FP-6", "completed", "done — hidden"),
                issue("FP-7", "canceled",  "killed — hidden"),
            ]
        }));
        let block = render_linear_block(&cache, "firefly-pro").expect("active issues → block");
        assert!(block.contains("📌 [Linear] Firefly Pro Beta — 5 active (1 in progress)"), "{block}");
        assert!(block.contains("🔄 FP-1 in flight"), "{block}");
        assert!(block.contains("⏳ FP-2 queued"), "{block}");
        assert!(block.contains("🗄️ FP-3 later"), "{block}");
        assert!(block.contains("🔺 FP-4 needs triage"), "{block}");
        assert!(block.contains("• FP-5 unknown non-terminal — shown"), "{block}");
        assert!(!block.contains("FP-6"), "completed hidden: {block}");
        assert!(!block.contains("FP-7"), "canceled hidden: {block}");
    }

    #[test]
    fn linear_orders_started_todo_backlog_triage() {
        let cache = cache_json(serde_json::json!({
            "initiative": "Init",
            "issues": [
                issue("T-1", "triage",    "d"),
                issue("B-1", "backlog",   "c"),
                issue("U-1", "unstarted", "b"),
                issue("S-1", "started",   "a"),
            ]
        }));
        let block = render_linear_block(&cache, "p").expect("block");
        let s = block.find("S-1").unwrap();
        let u = block.find("U-1").unwrap();
        let b = block.find("B-1").unwrap();
        let t = block.find("T-1").unwrap();
        assert!(s < u && u < b && b < t, "started → todo → backlog → triage: {block}");
    }

    #[test]
    fn linear_all_done_or_empty_renders_nothing() {
        // No active issues at all.
        let done = cache_json(serde_json::json!({
            "initiative": "Init",
            "issues": [issue("X-1", "completed", "done"), issue("X-2", "canceled", "gone")]
        }));
        assert!(render_linear_block(&done, "p").is_none(), "all-inactive → silent");
        // Empty issue list.
        let empty = cache_json(serde_json::json!({"initiative": "Init", "issues": []}));
        assert!(render_linear_block(&empty, "p").is_none(), "empty → silent");
    }

    #[test]
    fn linear_estimate_rendered_as_hint_and_header_falls_back() {
        // No initiative in the cache → header uses the passed project name.
        let cache = cache_json(serde_json::json!({
            "issues": [{
                "identifier": "FP-9", "title": "sized work", "estimate": 5,
                "state": {"type": "started", "name": "In Progress"}
            }]
        }));
        let block = render_linear_block(&cache, "firefly-pro").expect("block");
        assert!(block.contains("📌 [Linear] firefly-pro —"), "header fallback: {block}");
        assert!(block.contains("🔄 FP-9 sized work (5)"), "estimate hint: {block}");
    }

    #[test]
    fn linear_overflow_summarised_after_cap() {
        let issues: Vec<serde_json::Value> = (1..=12)
            .map(|i| issue(&format!("FP-{i}"), "unstarted", &format!("issue {i}")))
            .collect();
        let cache = cache_json(serde_json::json!({"initiative": "Init", "issues": issues}));
        let block = render_linear_block(&cache, "p").expect("block");
        let listed = block.matches("⏳ FP-").count();
        assert_eq!(listed, MAX_LINEAR_LISTED, "only the cap is listed: {block}");
        assert!(block.contains("…and 4 more active"), "{block}");
    }

    #[test]
    fn linear_title_truncation_is_char_boundary_safe() {
        let long = "│".repeat(200);
        let cache = cache_json(serde_json::json!({
            "initiative": "Init",
            "issues": [{
                "identifier": "FP-L", "title": long,
                "state": {"type": "started", "name": "s"}
            }]
        }));
        let block = render_linear_block(&cache, "p").expect("block");
        assert!(block.contains('…'), "long title ellipsised: {block}");
        let line = block.lines().find(|l| l.contains("FP-L")).unwrap();
        assert_eq!(
            line.chars().filter(|&c| c == '│').count(),
            MAX_SUBJECT_CHARS,
            "truncated to the char cap"
        );
    }

    /// Seed `<home>/.claude/sentinel/projects/firefly-pro.md` (Linear-backed)
    /// and the `linear-assigned.json` cache, returning the temp home fs.
    fn seed_firefly(tmp: &Path, cache_issues: serde_json::Value) {
        let proj_dir = tmp.join(".claude").join("sentinel").join("projects");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join("firefly-pro.md"),
            "---\nname: firefly-pro\naliases: [\"firefly\", \"crm\", \"fir\"]\nlinear_workspace: firefly-pro\nissue_prefix: FPCRM\n---\nbody\n",
        )
        .unwrap();
        // A non-Linear project must NOT trigger the overlay.
        std::fs::write(
            proj_dir.join("plainrepo.md"),
            "---\nname: plainrepo\naliases: [\"plain\"]\n---\nno linear here\n",
        )
        .unwrap();
        let cache = serde_json::json!({"initiative": "Firefly Pro Beta", "issues": cache_issues});
        std::fs::write(
            tmp.join(".claude").join("sentinel").join("linear-assigned.json"),
            serde_json::to_string(&cache).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn overlay_shows_in_linear_project_and_stacks_under_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        seed_firefly(
            tmp.path(),
            serde_json::json!([issue("FPCRM-622", "started", "auto move deal")]),
        );

        // Seed a native task so BOTH blocks render, stacked.
        let sid = "ovl-sess";
        let tdir = tmp.path().join(".claude").join("tasks").join(sid);
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(
            tdir.join("1.json"),
            r#"{"id":"1","subject":"native task","status":"in_progress"}"#,
        )
        .unwrap();

        // cwd matches the firefly alias segment → overlay fires.
        let input = HookInput {
            session_id: Some(sid.to_string()),
            cwd: Some("/c/Users/garys/Documents/GitHub/firefly".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        let s = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("both blocks must inject");
        assert!(s.contains("📋 [Tasks]"), "task block present: {s}");
        assert!(s.contains("📌 [Linear] Firefly Pro Beta"), "linear block present: {s}");
        assert!(s.contains("🔄 FPCRM-622 auto move deal"), "{s}");
        // Task block must precede the Linear block.
        assert!(s.find("📋 [Tasks]").unwrap() < s.find("📌 [Linear]").unwrap(), "{s}");
    }

    #[test]
    fn overlay_silent_in_non_linear_project() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        seed_firefly(
            tmp.path(),
            serde_json::json!([issue("FPCRM-1", "started", "x")]),
        );
        // cwd is the NON-Linear project → no Linear block, and no tasks → silent.
        let input = HookInput {
            session_id: Some("no-ovl".to_string()),
            cwd: Some("/c/Users/garys/Documents/GitHub/plainrepo".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(
            out.hook_specific_output.is_none(),
            "Linear overlay must stay silent outside a Linear-backed project"
        );
    }

    #[test]
    fn overlay_silent_when_cache_absent_even_in_linear_project() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        // Seed ONLY the project config, not the cache file.
        let proj_dir = tmp.path().join(".claude").join("sentinel").join("projects");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join("firefly-pro.md"),
            "---\nname: firefly-pro\naliases: [\"firefly\"]\nlinear_workspace: firefly-pro\n---\n",
        )
        .unwrap();
        let input = HookInput {
            session_id: Some("nocache".to_string()),
            cwd: Some("/x/firefly".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(
            out.hook_specific_output.is_none(),
            "absent cache in a Linear project must not panic and must stay silent"
        );
    }

    #[test]
    fn cwd_is_linear_backed_project_requires_both_linear_and_cwd_match() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        seed_firefly(tmp.path(), serde_json::json!([]));
        // firefly (Linear + cwd match) → Some(name)
        assert_eq!(
            cwd_is_linear_backed_project(&fs, "/x/firefly/sub"),
            Some("firefly-pro".to_string())
        );
        // plainrepo cwd match but NOT Linear-backed → None
        assert_eq!(cwd_is_linear_backed_project(&fs, "/x/plainrepo"), None);
        // Linear project but cwd matches nothing → None
        assert_eq!(cwd_is_linear_backed_project(&fs, "/x/unrelated"), None);
    }
}
