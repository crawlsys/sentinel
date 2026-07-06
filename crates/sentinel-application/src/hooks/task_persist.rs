//! Task Persist Hook — snapshot task list to project-level tasks.md + global JSON
//!
//! Fires on `TaskCreated`, `TaskCompleted`, and Stop events.
//!
//! Two outputs per fire:
//!
//! 1. **`<repo_root>/tasks.md`** (single source of truth, project-scoped)
//!    Wrapped in `<!-- SENTINEL:TASKS:START -->` … `<!-- SENTINEL:TASKS:END -->`
//!    markers so that hand-written content above/below the block is preserved.
//!    If the file doesn't exist it is created. If the file exists without markers
//!    a fresh marker block is prepended (and the existing content stays below it).
//!
//! 2. **`~/.claude/sentinel/persistent-tasks/{project_hash}/tasks.json`** (rehydration)
//!    Machine-readable snapshot consumed by `task_rehydrate` on `SessionStart`.
//!    `meta.json` next to it tracks last update + content hash for skip-if-unchanged.
//!    This is the only persistent task snapshot location; old non-sentinel paths
//!    are not read or migrated.
//!
//! Project scoping:
//!   - Repo root resolution via `GitStatusPort::repo_root(cwd)`. If the cwd is
//!     outside any git repo, the markdown write is skipped (only the global JSON
//!     snapshot is written, since there's no project root to anchor on).
//!   - The `project_hash` keying the global snapshot is SHA-256(cwd)[..4]; this
//!     matches `task_rehydrate.rs` so rehydration paths stay aligned.

use std::fmt::Write as _;

use chrono::Utc;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::{FileSystemPort, GitStatusPort, HookContext};

/// Public so `tasks_md_guard` / `linear_sync` can detect the auto block.
pub const MARKER_START: &str = "<!-- SENTINEL:TASKS:START -->";
pub const MARKER_END: &str = "<!-- SENTINEL:TASKS:END -->";

/// Linear issue from the project-scoped cache file at
/// `~/.claude/sentinel/linear-assigned-{project}.json`. The cache is populated
/// by the Linear refresh cron — this module only reads it.
#[derive(Debug, Clone, serde::Deserialize)]
struct LinearIssue {
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status_type: String,
    #[serde(default)]
    priority: serde_json::Value,
    #[serde(default)]
    url: String,
}

/// A single checklist item within a task
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ChecklistItem {
    id: String,
    text: String,
    #[serde(default)]
    completed: bool,
}

/// A task read from Claude Code's on-disk format
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Task {
    id: String,
    subject: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "activeForm")]
    active_form: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default, rename = "blockedBy")]
    blocked_by: Vec<String>,
    #[serde(default)]
    checklist: Vec<ChecklistItem>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

/// Persistent task snapshot metadata
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct PersistMeta {
    project_hash: String,
    cwd: String,
    session_id: String,
    updated_at: String,
    task_count: usize,
    incomplete_count: usize,
    /// Hex-encoded SHA-256 of the rendered auto-block — used to skip writes
    /// when nothing changed since the last snapshot.
    #[serde(default)]
    last_block_hash: String,
}

/// Compute a project hash from the working directory. Delegates to the shared
/// canonical implementation in `super::project_hash` so worktrees of the same
/// repo collapse to the same hash.
fn project_hash(cwd: &str) -> String {
    super::project_hash(cwd)
}

/// Hex-encode SHA-256 of a string. Used for content-hash skip checks.
fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let result = hasher.finalize();
    result.iter().fold(String::new(), |mut s, b| {
        write!(s, "{b:02x}").unwrap();
        s
    })
}

/// Get the persistent tasks directory for a project (under
/// `~/.claude/sentinel/persistent-tasks/`).
fn persistent_tasks_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(super::persistent_tasks_root(&home).join(project_hash))
}

/// Find the active task list directory for this session.
///
/// Strictly scoped to `~/.claude/tasks/{session_id}/`. Returns `None` if that
/// directory doesn't exist or has no task files — callers must treat `None`
/// as "nothing to persist".
///
/// No fallback: scanning `~/.claude/tasks/` for the most recently modified
/// dir leaks tasks across projects. A session in project A would inherit
/// project B's tasks if A's session dir hadn't been created yet.
fn find_active_task_dir(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let session_dir = super::session_task_dir(fs, &home, session_id);
    if fs.is_dir(&session_dir) && has_task_files(fs, &session_dir) {
        Some(session_dir)
    } else {
        None
    }
}

/// Check if a directory contains at least one .json task file (not .lock, not .highwatermark)
fn has_task_files(fs: &dyn FileSystemPort, dir: &PathBuf) -> bool {
    fs.read_dir(dir).is_ok_and(|entries| {
        entries.iter().any(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            std::path::Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                && !name.starts_with('.')
        })
    })
}

/// Status/priority decoration glyphs that a caller may have baked into a task
/// subject string (e.g. `"🔄 🔴 1 [P0] — Fix…"`). Kept in sync with
/// `session_init::strip_status_priority_prefix`'s `DECOR_EMOJI`.
const DECOR_EMOJI: &[char] = &['🔄', '⏳', '✅', '❌', '🔴', '🟠', '🟡', '🟢'];

/// Infer a status from a leading status glyph, if present. Returns `None` for
/// priority-only or unknown glyphs.
fn status_from_glyph(subject: &str) -> Option<&'static str> {
    match subject.trim_start().chars().next()? {
        '🔄' => Some("in_progress"),
        '⏳' => Some("pending"),
        '✅' => Some("completed"),
        '❌' => Some("cancelled"),
        _ => None,
    }
}

/// Infer a `[P0]`..`[P3]` priority from a leading priority token or colour
/// glyph, if present. `🔴`=P0, `🟠`=P1, `🟡`=P2, `🟢`=P3; `[Pn]` wins over glyph.
fn priority_from_decoration(subject: &str) -> Option<String> {
    let s = subject.trim_start();
    // Prefer an explicit [Pn] token anywhere in the leading decoration run.
    for tok in ["[P0]", "[P1]", "[P2]", "[P3]"] {
        if s.contains(tok) {
            return Some(tok.trim_matches(['[', ']']).to_string());
        }
    }
    match s.chars().find(|c| ['🔴', '🟠', '🟡', '🟢'].contains(c))? {
        '🔴' => Some("P0".into()),
        '🟠' => Some("P1".into()),
        '🟡' => Some("P2".into()),
        '🟢' => Some("P3".into()),
        _ => None,
    }
}

/// Strip leading status/priority decoration a caller baked into a subject
/// string. Mirrors `session_init::strip_status_priority_prefix` (kept in the
/// two modules independently since neither exports it). Idempotent.
fn strip_subject_decoration(subject: &str) -> &str {
    let mut s = subject.trim_start();
    loop {
        let before = s;
        s = s.trim_start_matches(|c| DECOR_EMOJI.contains(&c)).trim_start();
        if let Some(rest) = s.strip_prefix('[') {
            if let Some(close) = rest.find(']') {
                let inner = &rest[..close];
                if inner.len() <= 3
                    && inner.starts_with('P')
                    && inner[1..].chars().all(|c| c.is_ascii_digit())
                {
                    s = rest[close + 1..].trim_start();
                }
            }
        }
        let trimmed_num = s.trim_start_matches(|c: char| c.is_ascii_digit());
        if trimmed_num.len() < s.len() && trimmed_num.starts_with([' ', '—', '-', ':']) {
            s = trimmed_num.trim_start();
        }
        s = s.trim_start_matches(['—', '-', ':']).trim_start();
        if s == before {
            break;
        }
    }
    s
}

/// Normalize a task read from disk: pull any status/priority decoration baked
/// into the subject string out into the proper fields, and clean the subject.
///
/// The **field is authoritative** — a glyph only fills a *blank* field, and
/// never overrides an explicit `status`/`priority`. This fixes the corruption
/// where a task was stored as `"🔄 🔴 1 [P0] — Fix…"` with `status: "pending"`
/// (glyph and field disagreeing): the subject is cleaned to `"Fix…"`, the
/// explicit `pending` status is kept, and priority `P0` is backfilled only if
/// `metadata.priority` was absent.
fn normalize_task(mut task: Task) -> Task {
    let raw = task.subject.clone();
    let clean = strip_subject_decoration(&raw);

    if task.status.trim().is_empty() {
        if let Some(inferred) = status_from_glyph(&raw) {
            task.status = inferred.to_string();
        }
    }

    let has_priority = task
        .metadata
        .as_ref()
        .and_then(|m| m.get("priority"))
        .and_then(|p| p.as_str())
        .is_some_and(|p| !p.trim().is_empty());
    if !has_priority {
        if let Some(prio) = priority_from_decoration(&raw) {
            let mut map = match task.metadata.take() {
                Some(serde_json::Value::Object(m)) => m,
                _ => serde_json::Map::new(),
            };
            map.insert("priority".to_string(), serde_json::Value::String(prio));
            task.metadata = Some(serde_json::Value::Object(map));
        }
    }

    if clean != raw {
        task.subject = clean.to_string();
    }
    task
}

/// Read all tasks from a task list directory
fn read_tasks(fs: &dyn FileSystemPort, dir: &PathBuf) -> Vec<Task> {
    let mut tasks = Vec::new();
    if let Ok(entries) = fs.read_dir(dir) {
        for path in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !std::path::Path::new(&name)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                || name.starts_with('.')
            {
                continue;
            }
            if let Ok(content) = fs.read_to_string(&path) {
                if let Ok(task) = serde_json::from_str::<Task>(&content) {
                    tasks.push(normalize_task(task));
                }
            }
        }
    }
    // Sort by numeric ID
    tasks.sort_by(|a, b| {
        let a_num: u32 = a.id.parse().unwrap_or(u32::MAX);
        let b_num: u32 = b.id.parse().unwrap_or(u32::MAX);
        a_num.cmp(&b_num)
    });
    tasks
}

/// Read Linear issues from the project-scoped cache. Returns an empty vec when
/// the cache doesn't exist (no Linear configured, cron hasn't run yet, etc).
fn read_linear_issues(fs: &dyn FileSystemPort, project_name: &str) -> Vec<LinearIssue> {
    let Some(home) = fs.home_dir() else {
        return Vec::new();
    };
    let path = home
        .join(".claude")
        .join("sentinel")
        .join(format!("linear-assigned-{project_name}.json"));
    let Ok(content) = fs.read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<LinearIssue>>(&content).unwrap_or_default()
}

fn linear_status_rank(s: &str) -> u8 {
    match s {
        "started" => 0,   // In Progress
        "unstarted" => 1, // Todo
        "backlog" => 2,
        "triage" => 3,
        _ => 9,
    }
}

fn linear_status_label(s: &str) -> &'static str {
    match s {
        "started" => "In Progress",
        "unstarted" => "Todo",
        "backlog" => "Backlog",
        "triage" => "Triage",
        "completed" => "Done",
        "canceled" => "Canceled",
        _ => "—",
    }
}

fn linear_priority_label(v: &serde_json::Value) -> &'static str {
    if let Some(s) = v.as_str() {
        return match s.to_ascii_lowercase().as_str() {
            "urgent" => "🔴",
            "high" => "🟠",
            "medium" => "🟡",
            "low" => "🟢",
            _ => "·",
        };
    }
    if let Some(n) = v.as_u64() {
        return match n {
            1 => "🔴",
            2 => "🟠",
            3 => "🟡",
            4 => "🟢",
            _ => "·",
        };
    }
    "·"
}

/// Render Linear-issue rows. Active states only (no completed/canceled);
/// sorted by `status_type` rank then identifier.
fn render_linear_section(issues: &[LinearIssue]) -> String {
    if issues.is_empty() {
        return String::new();
    }

    let mut sorted: Vec<&LinearIssue> = issues
        .iter()
        .filter(|i| !matches!(i.status_type.as_str(), "completed" | "canceled"))
        .collect();
    if sorted.is_empty() {
        return String::new();
    }
    sorted.sort_by(|a, b| {
        linear_status_rank(&a.status_type)
            .cmp(&linear_status_rank(&b.status_type))
            .then(a.identifier.cmp(&b.identifier))
    });

    let mut md = String::from("## Linear Assigned\n\n");
    writeln!(md, "_{} open issue(s) from Linear cache._\n", sorted.len()).unwrap();
    md.push_str("| Issue | Pri | State | Title |\n");
    md.push_str("|---|---|---|---|\n");
    for i in sorted.iter().take(50) {
        let id = if i.url.is_empty() {
            i.identifier.clone()
        } else {
            format!("[{}]({})", i.identifier, i.url)
        };
        writeln!(
            md,
            "| {} | {} | {} | {} |",
            id,
            linear_priority_label(&i.priority),
            linear_status_label(&i.status_type),
            i.title.replace('|', "\\|"),
        )
        .unwrap();
    }
    if sorted.len() > 50 {
        writeln!(md, "\n…and {} more (truncated).", sorted.len() - 50).unwrap();
    }
    md.push('\n');
    md
}

/// Render the body that lives **inside** the marker block. Excludes the markers
/// themselves so callers can splice it cleanly into existing files.
fn render_auto_block_body(
    tasks: &[Task],
    project_name: &str,
    linear_issues: &[LinearIssue],
) -> String {
    let now = Utc::now().to_rfc3339();
    let incomplete: Vec<&Task> = tasks.iter().filter(|t| t.status != "completed").collect();
    let completed: Vec<&Task> = tasks.iter().filter(|t| t.status == "completed").collect();
    let in_progress = incomplete
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();

    let mut md = String::new();
    md.push('\n');
    writeln!(md,
        "<!-- This block is auto-managed by sentinel `task_persist` hook. Edit via TaskCreate / Linear, not by hand. Updated: {now} -->\n"
    ).unwrap();
    writeln!(md, "# Tasks — {project_name}\n").unwrap();
    writeln!(
        md,
        "**{total} total · {pending} pending · {in_prog} in progress · {done} completed**\n",
        total = tasks.len(),
        pending = incomplete.len().saturating_sub(in_progress),
        in_prog = in_progress,
        done = completed.len(),
    )
    .unwrap();

    let linear_section = render_linear_section(linear_issues);

    if tasks.is_empty() {
        md.push_str("_No native tasks. Use `TaskCreate` to add one._\n\n");
        if !linear_section.is_empty() {
            md.push_str(&linear_section);
        }
        return md;
    }

    if !incomplete.is_empty() {
        md.push_str("## Open\n\n");
        for task in &incomplete {
            let check = match task.status.as_str() {
                "in_progress" => "~",
                _ => " ",
            };
            writeln!(md, "### [{check}] {}. {}", task.id, task.subject).unwrap();
            writeln!(md, "- **Status:** {}", task.status).unwrap();
            if !task.blocks.is_empty() {
                writeln!(md, "- **Blocks:** {}", task.blocks.join(", ")).unwrap();
            }
            if !task.blocked_by.is_empty() {
                writeln!(md, "- **Blocked by:** {}", task.blocked_by.join(", ")).unwrap();
            }
            if let Some(owner) = &task.owner {
                writeln!(md, "- **Owner:** {owner}").unwrap();
            }
            if let Some(meta) = &task.metadata {
                if let Some(obj) = meta.as_object() {
                    if let Some(priority) = obj.get("priority").and_then(|v| v.as_str()) {
                        writeln!(md, "- **Priority:** {priority}").unwrap();
                    }
                    if let Some(phase) = obj.get("phase").and_then(|v| v.as_str()) {
                        writeln!(md, "- **Phase:** {phase}").unwrap();
                    }
                    if let Some(tags) = obj.get("skill_tags").and_then(|v| v.as_array()) {
                        let tag_strs: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
                        if !tag_strs.is_empty() {
                            writeln!(md, "- **Tags:** {}", tag_strs.join(", ")).unwrap();
                        }
                    }
                }
            }
            if !task.description.is_empty() {
                writeln!(md, "- **Description:** {}", task.description).unwrap();
            }
            if !task.checklist.is_empty() {
                let done = task.checklist.iter().filter(|c| c.completed).count();
                writeln!(md, "- **Checklist:** ({}/{})", done, task.checklist.len()).unwrap();
                for item in &task.checklist {
                    let mark = if item.completed { "x" } else { " " };
                    writeln!(md, "  - [{mark}] {}", item.text).unwrap();
                }
            }
            md.push('\n');
        }
    }

    if !completed.is_empty() {
        md.push_str("## Completed\n\n");
        for task in &completed {
            writeln!(md, "- [x] **{}. {}**", task.id, task.subject).unwrap();
        }
        md.push('\n');
    }

    if !linear_section.is_empty() {
        md.push_str(&linear_section);
    }

    md
}

/// Wrap a body string with the marker pair.
fn wrap_in_markers(body: &str) -> String {
    format!("{MARKER_START}\n{body}{MARKER_END}\n")
}

/// Splice or insert the auto block into existing file content.
///
/// Three cases:
/// - existing has both markers → replace block between them (preserving everything else)
/// - existing has no markers → prepend a new wrapped block, keep existing content below
/// - empty/missing existing → just the wrapped block
fn merge_with_existing(existing: Option<&str>, body: &str) -> String {
    let wrapped = wrap_in_markers(body);
    match existing {
        None | Some("") => wrapped,
        Some(content) => match (content.find(MARKER_START), content.find(MARKER_END)) {
            (Some(s), Some(e)) if e > s => {
                // Replace [s, e + MARKER_END.len())
                let before = &content[..s];
                let end_idx = e + MARKER_END.len();
                // Skip a trailing newline after MARKER_END so we don't accumulate blank lines.
                let after = content[end_idx..]
                    .strip_prefix('\n')
                    .unwrap_or_else(|| &content[end_idx..]);
                format!("{before}{wrapped}{after}")
            }
            _ => {
                // No markers — prepend the auto block, keep existing content below it
                // separated by a blank line so the user's prior content is undisturbed.
                let sep = if content.starts_with('\n') { "" } else { "\n" };
                format!("{wrapped}{sep}{content}")
            }
        },
    }
}

/// Resolve the project repo root for a given cwd, falling back to `None` when
/// the path is outside any git repo.
fn project_repo_root(git: &dyn GitStatusPort, cwd: &str) -> Option<PathBuf> {
    git.repo_root(cwd).map(PathBuf::from)
}

/// Encode an absolute path into the project-key format used by Claude Code's
/// `~/.claude/projects/<key>/` directory: strip drive colon, replace `\` and
/// `/` with `-`, leave consecutive separators (i.e. `C:\` becomes `C--`).
fn encode_project_key(path: &str) -> String {
    path.chars()
        .map(|c| match c {
            '\\' | '/' | ':' => '-',
            _ => c,
        })
        .collect()
}

/// Pull the project name (last segment of repo root path) for the rendered header.
fn project_name(repo_root: &Path) -> String {
    repo_root.file_name().map_or_else(
        || "project".to_string(),
        |n| n.to_string_lossy().to_string(),
    )
}

/// Atomic file replacement for Sentinel task authority snapshots.
fn atomic_write(fs: &dyn FileSystemPort, path: &Path, content: &str) -> anyhow::Result<()> {
    fs.replace_file_atomic(path, content.as_bytes())
        .map_err(anyhow::Error::from)
}

/// Minimum task count in an existing block before the shrink guard engages.
/// Smaller blocks can shrink freely — false-alarming on the first-few-tasks
/// case (where a brand-new project legitimately drops from 5 to 3 tasks) is
/// worse than the marginal protection it provides.
const SHRINK_GUARD_MIN_EXISTING: usize = 10;

/// Ratio threshold: if the new render has fewer than this fraction of the
/// existing block's task count, treat it as suspicious and refuse to write.
/// 0.5 means "new must be at least half of existing"; a small session that
/// hadn't rehydrated prior state would typically have 1-5 tasks vs. an
/// accumulated block of 80+, which sits far below this floor.
const SHRINK_GUARD_RATIO: f64 = 0.5;

/// Env var that bypasses [`SHRINK_GUARD_RATIO`] for the cases where the user
/// genuinely intends to shrink the list (bulk cleanup, project pivot).
/// Setting it to any non-empty value force-writes the new (smaller) block.
const SHRINK_GUARD_FORCE_ENV: &str = "SENTINEL_FORCE_TASKS_MD_WRITE";

/// Count rendered tasks inside an auto-block body. Incomplete tasks render
/// as `### [<mark>] <N>.` headers; completed tasks render as
/// `- [x] **<N>.` lines. The sum is the block's total task count.
///
/// Used by the shrink guard in [`write_project_tasks_md`] to decide whether
/// the new render is a routine update or a suspicious wipe.
fn count_block_tasks(body: &str) -> usize {
    let open_count = body
        .lines()
        .filter(|l| l.starts_with("### [") && l.contains("] ") && l.contains(". "))
        .count();
    let completed_count = body
        .lines()
        .filter(|l| l.starts_with("- [x] **") && l.contains(". "))
        .count();
    open_count + completed_count
}

/// Extract the marker block body from existing tasks.md content. Returns
/// `None` when the markers are absent (no prior auto-block to compare to).
fn extract_existing_block(content: &str) -> Option<&str> {
    let start = content.find(MARKER_START)?;
    let end = content.find(MARKER_END)?;
    if end <= start {
        return None;
    }
    Some(&content[start + MARKER_START.len()..end])
}

/// Write `<repo_root>/tasks.md` with the auto block merged into existing content.
///
/// **Shrink guard (2026-05-16)**: when the new render would replace an
/// existing block that has >= [`SHRINK_GUARD_MIN_EXISTING`] tasks with one
/// that has fewer than half as many, the write is refused and a `tracing::warn`
/// is logged. This prevents the failure mode where a brand-new session with
/// a small `TaskList` overwrites a `tasks.md` block accumulated across many
/// prior sessions (the user's tasks visibly vanish from the auto-block).
/// Set `SENTINEL_FORCE_TASKS_MD_WRITE=1` to override.
///
/// Returns `true` if the file was actually changed, `false` if skipped
/// (no diff, or shrink guard engaged).
fn write_project_tasks_md(
    fs: &dyn FileSystemPort,
    repo_root: &Path,
    body: &str,
) -> anyhow::Result<bool> {
    let path = repo_root.join("tasks.md");
    let existing = fs.read_to_string(&path).ok();

    // Shrink guard: compare current block size to new render size.
    if let Some(prev) = &existing {
        if let Some(existing_block) = extract_existing_block(prev) {
            let existing_count = count_block_tasks(existing_block);
            let new_count = count_block_tasks(body);
            let force = std::env::var(SHRINK_GUARD_FORCE_ENV).is_ok_and(|v| !v.is_empty());
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let allowed_min = (existing_count as f64 * SHRINK_GUARD_RATIO).ceil() as usize;
            if !force && existing_count >= SHRINK_GUARD_MIN_EXISTING && new_count < allowed_min {
                tracing::warn!(
                    repo_root = %repo_root.display(),
                    existing_count,
                    new_count,
                    allowed_min,
                    env_override = SHRINK_GUARD_FORCE_ENV,
                    "task_persist: shrink guard engaged — refusing to wipe tasks.md \
                     (new render has {new_count} tasks vs. existing {existing_count}). \
                     Set {SHRINK_GUARD_FORCE_ENV}=1 to force the write."
                );
                return Ok(false);
            }
        }
    }

    let merged = merge_with_existing(existing.as_deref(), body);

    // Skip if no change.
    if let Some(prev) = &existing {
        if prev == &merged {
            return Ok(false);
        }
    }

    atomic_write(fs, &path, &merged)?;
    Ok(true)
}

/// Write a project-tasks summary into Claude Code's native memory dir.
///
/// Path: `~/.claude/projects/<project-key>/memory/project_tasks.md` (the same
/// dir that gets loaded as `# auto memory` at session start). The `<project-key>`
/// encodes the **repo root**, not the worktree cwd, so worktree sessions
/// update the canonical project entry instead of fragmenting per branch.
///
/// Skips silently when home dir is unavailable, when the projects directory
/// for this repo doesn't exist (Claude Code creates it lazily on first use),
/// or when the rendered body matches the prior write.
fn write_memory_summary(
    fs: &dyn FileSystemPort,
    repo_root: &Path,
    tasks: &[Task],
) -> anyhow::Result<bool> {
    let Some(home) = fs.home_dir() else {
        return Ok(false);
    };
    let key = encode_project_key(&repo_root.to_string_lossy());
    let project_dir = home.join(".claude").join("projects").join(&key);
    if !fs.is_dir(&project_dir) {
        // Claude Code hasn't materialized this project yet — nothing to update.
        tracing::debug!(
            project_key = %key,
            "Skipping memory writeback: ~/.claude/projects/<key> not present"
        );
        return Ok(false);
    }
    let memory_dir = project_dir.join("memory");
    fs.create_dir_all(&memory_dir)?;

    let proj_name = project_name(repo_root);
    let incomplete: Vec<&Task> = tasks.iter().filter(|t| t.status != "completed").collect();
    let in_progress = incomplete
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();

    let mut body = String::new();
    body.push_str("---\n");
    writeln!(body, "name: Tasks for {proj_name}").unwrap();
    writeln!(
        body,
        "description: Snapshot of native TaskList for {proj_name} — kept in sync by sentinel `task_persist` hook on TaskCreated/TaskCompleted/Stop. {open} open, {ip} in progress.",
        open = incomplete.len(),
        ip = in_progress,
    ).unwrap();
    body.push_str("type: project\n");
    body.push_str("source: auto\n");
    body.push_str("---\n\n");
    writeln!(
        body,
        "**{open}** open · **{ip}** in progress · **{done}** completed (full state in `tasks.md` at the repo root)\n",
        open = incomplete.len(),
        ip = in_progress,
        done = tasks.len() - incomplete.len(),
    ).unwrap();

    if incomplete.is_empty() {
        body.push_str("_No open tasks._\n");
    } else {
        body.push_str("Top open tasks (by id):\n\n");
        for task in incomplete.iter().take(10) {
            let mark = if task.status == "in_progress" {
                "~"
            } else {
                " "
            };
            write!(body, "- [{mark}] **#{}** {}", task.id, task.subject).unwrap();
            if !task.blocked_by.is_empty() {
                write!(body, " _(blocked by {})_", task.blocked_by.join(", ")).unwrap();
            }
            body.push('\n');
        }
        if incomplete.len() > 10 {
            writeln!(body, "- _…and {} more._", incomplete.len() - 10).unwrap();
        }
    }

    let memory_file = memory_dir.join("project_tasks.md");
    let prior = fs.read_to_string(&memory_file).ok();
    if prior.as_deref() == Some(body.as_str()) {
        return Ok(false);
    }
    fs.write(&memory_file, body.as_bytes())?;

    // Add/update one line in MEMORY.md index. Idempotent: replaces an existing
    // `- [Tasks for …](project_tasks.md)` line if present, else appends.
    let index_path = memory_dir.join("MEMORY.md");
    let new_index_line = format!(
        "- [Tasks for {proj_name}](project_tasks.md) — {open} open, {ip} in progress\n",
        open = incomplete.len(),
        ip = in_progress,
    );
    let updated_index = match fs.read_to_string(&index_path).ok() {
        Some(existing) => {
            let mut found = false;
            let mut out = String::with_capacity(existing.len() + new_index_line.len());
            for line in existing.lines() {
                if line.starts_with("- [Tasks for ") && line.contains("](project_tasks.md)") {
                    out.push_str(&new_index_line);
                    found = true;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if !found {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str(&new_index_line);
            }
            out
        }
        None => new_index_line,
    };
    fs.write(&index_path, updated_index.as_bytes())?;

    Ok(true)
}

/// Persist tasks to disk: project-level markdown (if in a repo) + global JSON snapshot.
fn write_persistent_tasks(
    fs: &dyn FileSystemPort,
    git: &dyn GitStatusPort,
    tasks: &[Task],
    cwd: &str,
    proj_hash: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    let global_dir = match persistent_tasks_dir(fs, proj_hash) {
        Some(d) => d,
        None => return Ok(()),
    };
    fs.create_dir_all(&global_dir)?;

    // Always update the global JSON snapshot (used by task_rehydrate).
    //
    // **Bug fix (2026-05-06)**: Previously did a direct fs.write(), which is
    // NOT atomic — a kill / crash / laptop-close mid-write leaves tasks.json
    // half-written. Next SessionStart's task_rehydrate sees malformed JSON,
    // serde_json::from_str fails, .ok()? converts to None, the rehydrator
    // silently injects nothing. The user's task list APPEARS empty in the new
    // session even though all 70 tasks "existed" five minutes ago. This is
    // the worst-case data-loss path in the whole system.
    //
    // Now: serialize first, then replace through the filesystem port's
    // first-class atomic replacement operation. The existing good tasks.json is
    // preserved if serialization or replacement fails.
    //
    // **Refuse to write empty/invalid JSON**: if serialization somehow yields
    // an empty string, refuse to clobber the prior good snapshot. Better to
    // return an error and keep the old data than overwrite with garbage.
    let json = serde_json::to_string_pretty(tasks)
        .map_err(|e| anyhow::anyhow!("serialize tasks for persistence: {e}"))?;
    if json.trim().is_empty() || !json.trim_start().starts_with('[') {
        return Err(anyhow::anyhow!(
            "refusing to write malformed tasks.json (would clobber prior good snapshot)"
        ));
    }
    atomic_write(fs, &global_dir.join("tasks.json"), &json)?;

    // Write the project-level tasks.md when we're in a git repo.
    let repo_root = project_repo_root(git, cwd);
    let mut block_hash = String::new();
    if let Some(root) = &repo_root {
        let proj_name = project_name(root);
        let linear_issues = read_linear_issues(fs, &proj_name);
        let body = render_auto_block_body(tasks, &proj_name, &linear_issues);
        block_hash = sha256_hex(&body);

        // Skip-if-unchanged: read prior hash from meta.json.
        let prior_hash = fs
            .read_to_string(&global_dir.join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<PersistMeta>(&s).ok())
            .map(|m| m.last_block_hash)
            .unwrap_or_default();

        if prior_hash != block_hash {
            if let Err(e) = write_project_tasks_md(fs, root, &body) {
                tracing::warn!(error = %e, repo_root = %root.display(), "Failed to write tasks.md");
            }
            if let Err(e) = write_memory_summary(fs, root, tasks) {
                tracing::warn!(error = %e, "Failed to write project_tasks memory file");
            }
        }
    } else {
        tracing::debug!(
            cwd,
            "Not inside a git repo — skipping project tasks.md write"
        );
    }

    // Write meta.json (always — captures last_block_hash for the next compare).
    let incomplete_count = tasks.iter().filter(|t| t.status != "completed").count();
    let meta = PersistMeta {
        project_hash: proj_hash.to_string(),
        cwd: cwd.to_string(),
        session_id: session_id.to_string(),
        updated_at: Utc::now().to_rfc3339(),
        task_count: tasks.len(),
        incomplete_count,
        last_block_hash: block_hash,
    };
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| anyhow::anyhow!("serialize meta for persistence: {e}"))?;
    // **Bug fix (2026-05-06)**: atomic write — same reasoning as tasks.json
    // above. A half-written meta.json with truncated JSON would make
    // task_rehydrate's read_meta() return None silently, defeating the
    // is_current_session() check and over-rehydrating tasks the user has
    // already worked on this session.
    atomic_write(fs, &global_dir.join("meta.json"), &meta_json)?;

    tracing::debug!(
        project_hash = proj_hash,
        task_count = tasks.len(),
        incomplete_count,
        in_repo = repo_root.is_some(),
        "Persisted tasks to disk"
    );

    Ok(())
}

/// Truncate the persistent task snapshot when the live task list is empty.
///
/// The counterpart to [`write_persistent_tasks`] for the "list is now empty"
/// state. Without this, a `tasks.json` written while the list was non-empty
/// stays frozen on disk once the list empties — driving phantom
/// `task_coverage_check` nags, ghost rows in CLAUDE.md's Active Tasks table,
/// and spurious `task_rehydrate` re-injection on the next SessionStart.
///
/// Behaviour:
/// - **No-op when there is nothing to clear.** If no prior `tasks.json` exists
///   (a project that never had tasks), we return without writing — truncation
///   only matters where a stale snapshot is present.
/// - Writes `tasks.json` = `[]` and `meta.json` with `task_count: 0` atomically
///   (same crash-safety reasoning as the non-empty path).
/// - Replaces the `tasks.md` marker block with an empty auto-block, preserving
///   the user's hand-written content above/below. This bypasses the shrink
///   guard on purpose: going to zero here is the *intended* state, not the
///   accidental small-`TaskList`-clobbers-big-block case the guard defends.
fn truncate_persistent_tasks(
    fs: &dyn FileSystemPort,
    git: &dyn GitStatusPort,
    cwd: &str,
    proj_hash: &str,
    session_id: &str,
) -> anyhow::Result<()> {
    let global_dir = match persistent_tasks_dir(fs, proj_hash) {
        Some(d) => d,
        None => return Ok(()),
    };

    let json_path = global_dir.join("tasks.json");

    // Nothing to clear: no prior snapshot, or it is already empty. Reading the
    // file and checking for a non-empty array avoids a pointless write (and a
    // spurious meta.json mtime bump) every Stop fire on a project that has no
    // tasks and never did.
    let prior = fs.read_to_string(&json_path).ok();
    let already_empty = match &prior {
        None => true,
        Some(s) => serde_json::from_str::<Vec<Task>>(s)
            .map(|v| v.is_empty())
            .unwrap_or(false),
    };
    if already_empty {
        return Ok(());
    }

    fs.create_dir_all(&global_dir)?;

    // Empty JSON array — passes the `starts_with('[')` guard in the non-empty
    // path's writer, and is exactly what `task_rehydrate` reads as "no tasks".
    atomic_write(fs, &json_path, "[]")?;

    // Replace the tasks.md auto-block with an empty body (markers retained).
    let repo_root = project_repo_root(git, cwd);
    if let Some(root) = &repo_root {
        let proj_name = project_name(root);
        let empty_body = render_auto_block_body(&[], &proj_name, &[]);
        let path = root.join("tasks.md");
        if let Ok(existing) = fs.read_to_string(&path) {
            let merged = merge_with_existing(Some(&existing), &empty_body);
            if merged != existing {
                if let Err(e) = atomic_write(fs, &path, &merged) {
                    tracing::warn!(error = %e, repo_root = %root.display(),
                        "Failed to clear tasks.md auto-block on truncate");
                }
            }
        }
    }

    // Reset meta.json to a zero-count snapshot.
    let meta = PersistMeta {
        project_hash: proj_hash.to_string(),
        cwd: cwd.to_string(),
        session_id: session_id.to_string(),
        updated_at: Utc::now().to_rfc3339(),
        task_count: 0,
        incomplete_count: 0,
        last_block_hash: String::new(),
    };
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| anyhow::anyhow!("serialize meta for truncate: {e}"))?;
    atomic_write(fs, &global_dir.join("meta.json"), &meta_json)?;

    tracing::info!(
        project_hash = proj_hash,
        "Truncated stale task snapshot — live task list is empty"
    );
    Ok(())
}

/// Process task persistence on `TaskCreated`, `TaskCompleted`, or Stop events.
///
/// Reads the active session's task files, then writes:
/// - `<repo_root>/tasks.md` (project-scoped, with marker block)
/// - `~/.claude/sentinel/persistent-tasks/{project_hash}/tasks.json` (rehydration source)
/// - `~/.claude/sentinel/persistent-tasks/{project_hash}/meta.json` (skip-if-unchanged)
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // **Bug fix (2026-05-06)**: Previously fell back to "unknown" silently
    // when input.session_id was absent. Then find_active_task_dir() would
    // search ~/.claude/tasks/unknown/ — which never exists — and bail with
    // "no active task directory" while emitting a debug log nobody sees.
    // Result: tasks created in-memory this session NEVER persisted to disk,
    // and you only discovered it the next time you opened a new session and
    // saw a much shorter task list than expected.
    //
    // Same shape as the hook_cmd.rs fix: prefer input.session_id, then
    // $CLAUDE_SESSION_ID env var, then refuse with a tracing::warn so the
    // failure is visible. Returning HookOutput::allow() unblocks the tool
    // call (we never want to block a user action over a persistence issue),
    // but the warn surfaces the durability gap.
    let session_id = match input.session_id.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => match std::env::var("CLAUDE_SESSION_ID") {
            Ok(s) if !s.is_empty() => s,
            _ => {
                tracing::warn!(
                    "task_persist: no session_id (input.session_id absent and \
                     CLAUDE_SESSION_ID env var unset). Tasks will NOT be \
                     persisted to disk this fire — durability gap. Investigate \
                     why HookInput is missing session_id."
                );
                return HookOutput::allow();
            }
        },
    };
    let session_id: &str = &session_id;
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let proj_hash = project_hash(cwd);

    // Resolve the live source of truth. `None` = the session's task dir is
    // absent; an empty vec = the dir exists but holds no task files. BOTH mean
    // "the live task list is empty right now".
    //
    // **Ghost-snapshot fix (2026-07-06)**: previously both cases early-returned
    // *without touching the snapshot*, so a `tasks.json` written when the list
    // was non-empty stayed frozen on disk forever once the list emptied. That
    // stale file then (a) drove `task_coverage_check` to nag about tasks that
    // no longer exist, (b) rendered phantom rows in CLAUDE.md's Active Tasks
    // table, and (c) got re-injected by `task_rehydrate` on the next
    // SessionStart. The list going to zero is a real state that MUST be
    // mirrored — truncate the snapshot instead of leaving ghosts.
    //
    // Safety: we only reach here with a *valid, non-empty* session_id (the
    // durability-gap guard above already returned on a missing one), so an
    // empty read here is a genuine "no live tasks", not a resolution failure.
    let tasks = match find_active_task_dir(ctx.fs, session_id) {
        Some(dir) => read_tasks(ctx.fs, &dir),
        None => Vec::new(),
    };

    if tasks.is_empty() {
        if let Err(e) = truncate_persistent_tasks(ctx.fs, ctx.git, cwd, &proj_hash, session_id) {
            tracing::warn!(error = %e, "Failed to truncate stale task snapshot");
        }
        return HookOutput::allow();
    }

    if let Err(e) = write_persistent_tasks(ctx.fs, ctx.git, &tasks, cwd, &proj_hash, session_id) {
        tracing::warn!(error = %e, "Failed to persist tasks");
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::port_errors::GitError;
    use std::path::Path;

    /// Minimal real-FS for tests that need to read temp directories.
    struct TestFs;
    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> {
            dirs::home_dir()
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn replace_file_atomic(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            self.write(p, c)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            _: &Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/operator/projects/firefly");
        let h2 = project_hash("/Users/operator/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_project_hash_different() {
        let h1 = project_hash("/Users/operator/projects/firefly");
        let h2 = project_hash("/Users/operator/projects/corvus");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_render_auto_block_body_empty() {
        let body = render_auto_block_body(&[], "myproject", &[]);
        assert!(body.contains("# Tasks — myproject"));
        assert!(body.contains("_No native tasks"));
        assert!(body.contains("0 total"));
    }

    #[test]
    fn test_render_auto_block_body_with_linear_only() {
        let issues = vec![LinearIssue {
            identifier: "FPCRM-1".into(),
            title: "Fix the thing".into(),
            status_type: "started".into(),
            priority: serde_json::json!("high"),
            url: "https://linear.app/firefly-pro/issue/FPCRM-1/fix-the-thing".into(),
        }];
        let body = render_auto_block_body(&[], "firefly-pro", &issues);
        assert!(body.contains("## Linear Assigned"));
        assert!(body.contains("FPCRM-1"));
        assert!(body.contains("Fix the thing"));
        assert!(body.contains("In Progress"));
    }

    #[test]
    fn test_render_linear_section_skips_completed() {
        let issues = vec![
            LinearIssue {
                identifier: "X-1".into(),
                title: "open".into(),
                status_type: "started".into(),
                priority: serde_json::json!("medium"),
                url: String::new(),
            },
            LinearIssue {
                identifier: "X-2".into(),
                title: "shipped".into(),
                status_type: "completed".into(),
                priority: serde_json::json!("low"),
                url: String::new(),
            },
        ];
        let section = render_linear_section(&issues);
        assert!(section.contains("X-1"));
        assert!(
            !section.contains("X-2"),
            "completed issues must be filtered"
        );
        assert!(section.contains("1 open issue"));
    }

    #[test]
    fn test_render_linear_section_orders_started_first() {
        let issues = vec![
            LinearIssue {
                identifier: "B".into(),
                title: "backlog".into(),
                status_type: "backlog".into(),
                priority: serde_json::json!("low"),
                url: String::new(),
            },
            LinearIssue {
                identifier: "A".into(),
                title: "in progress".into(),
                status_type: "started".into(),
                priority: serde_json::json!("high"),
                url: String::new(),
            },
        ];
        let section = render_linear_section(&issues);
        let pos_started = section.find("in progress").unwrap();
        let pos_backlog = section.find("backlog").unwrap();
        assert!(
            pos_started < pos_backlog,
            "started rows must render before backlog"
        );
    }

    #[test]
    fn test_render_linear_section_empty_when_all_filtered() {
        let issues = vec![LinearIssue {
            identifier: "X".into(),
            title: "done".into(),
            status_type: "completed".into(),
            priority: serde_json::json!("low"),
            url: String::new(),
        }];
        assert_eq!(render_linear_section(&issues), "");
    }

    #[test]
    fn test_render_auto_block_body_with_tasks() {
        let tasks = vec![
            Task {
                id: "1".to_string(),
                subject: "Fix auth".to_string(),
                description: "OAuth2 flow".to_string(),
                active_form: None,
                owner: None,
                status: "in_progress".to_string(),
                blocks: vec!["2".to_string()],
                blocked_by: vec![],
                checklist: vec![
                    ChecklistItem {
                        id: "1".to_string(),
                        text: "Design API".to_string(),
                        completed: true,
                    },
                    ChecklistItem {
                        id: "2".to_string(),
                        text: "Write tests".to_string(),
                        completed: false,
                    },
                ],
                metadata: Some(serde_json::json!({
                    "priority": "P0",
                    "phase": "auth-refactor",
                    "skill_tags": ["feature", "security"]
                })),
            },
            Task {
                id: "2".to_string(),
                subject: "Write tests".to_string(),
                description: "Unit tests".to_string(),
                active_form: None,
                owner: None,
                status: "pending".to_string(),
                blocks: vec![],
                blocked_by: vec!["1".to_string()],
                checklist: vec![],
                metadata: None,
            },
            Task {
                id: "3".to_string(),
                subject: "Deploy".to_string(),
                description: "Push to prod".to_string(),
                active_form: None,
                owner: None,
                status: "completed".to_string(),
                blocks: vec![],
                blocked_by: vec![],
                checklist: vec![],
                metadata: None,
            },
        ];
        let body = render_auto_block_body(&tasks, "myproject", &[]);
        assert!(body.contains("# Tasks — myproject"));
        assert!(body.contains("[~] 1. Fix auth"));
        assert!(body.contains("[ ] 2. Write tests"));
        assert!(body.contains("[x] **3. Deploy**"));
        assert!(body.contains("**Blocks:** 2"));
        assert!(body.contains("**Blocked by:** 1"));
        assert!(body.contains("3 total"));
        assert!(body.contains("**Checklist:** (1/2)"));
        assert!(body.contains("**Priority:** P0"));
        assert!(body.contains("**Tags:** feature, security"));
    }

    #[test]
    fn test_merge_with_existing_no_file() {
        let body = "BODY\n";
        let merged = merge_with_existing(None, body);
        assert!(merged.contains(MARKER_START));
        assert!(merged.contains("BODY"));
        assert!(merged.contains(MARKER_END));
    }

    #[test]
    fn test_merge_with_existing_no_markers_preserves_existing() {
        let existing = "# My Roadmap\n\nThis is hand-written.\n";
        let body = "auto-body\n";
        let merged = merge_with_existing(Some(existing), body);
        // Auto block goes ABOVE existing content, preserving every original line.
        assert!(merged.starts_with(MARKER_START));
        assert!(merged.contains("auto-body"));
        assert!(merged.contains(MARKER_END));
        assert!(merged.contains("# My Roadmap"));
        assert!(merged.contains("This is hand-written."));
        // Hand-written content is below the auto block (i.e. found AFTER MARKER_END).
        let end_idx = merged.find(MARKER_END).unwrap();
        let tail = &merged[end_idx..];
        assert!(tail.contains("# My Roadmap"));
    }

    #[test]
    fn test_merge_with_existing_replaces_block_only() {
        let existing = format!(
            "# My Roadmap\n\nKeep this.\n\n{MARKER_START}\nold body\n{MARKER_END}\n\nKeep this too.\n"
        );
        let merged = merge_with_existing(Some(&existing), "new body\n");
        assert!(merged.contains("Keep this."));
        assert!(merged.contains("Keep this too."));
        assert!(merged.contains("new body"));
        assert!(!merged.contains("old body"));
        // Markers still present, exactly once each.
        assert_eq!(merged.matches(MARKER_START).count(), 1);
        assert_eq!(merged.matches(MARKER_END).count(), 1);
    }

    #[test]
    fn test_merge_with_existing_replaces_block_idempotent() {
        let body = "stable body\n";
        let first = merge_with_existing(None, body);
        let second = merge_with_existing(Some(&first), body);
        assert_eq!(first, second, "merging the same body twice must be stable");
    }

    #[test]
    fn test_read_tasks_sorted() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().to_path_buf();

        std::fs::write(
            dir.join("3.json"),
            r#"{"id":"3","subject":"Third","description":"","status":"pending","blocks":[],"blockedBy":[],"checklist":[]}"#,
        ).unwrap();
        std::fs::write(
            dir.join("1.json"),
            r#"{"id":"1","subject":"First","description":"","status":"pending","blocks":[],"blockedBy":[],"checklist":[]}"#,
        ).unwrap();
        std::fs::write(
            dir.join("2.json"),
            r#"{"id":"2","subject":"Second","description":"","status":"pending","blocks":[],"blockedBy":[],"checklist":[]}"#,
        ).unwrap();

        let fs = TestFs;
        let tasks = read_tasks(&fs, &dir);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].id, "1");
        assert_eq!(tasks[1].id, "2");
        assert_eq!(tasks[2].id, "3");
    }

    #[test]
    fn test_has_task_files() {
        let fs = TestFs;
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path().to_path_buf();

        assert!(!has_task_files(&fs, &dir));

        std::fs::write(dir.join(".lock"), "").unwrap();
        assert!(!has_task_files(&fs, &dir));

        std::fs::write(dir.join("1.json"), "{}").unwrap();
        assert!(has_task_files(&fs, &dir));
    }

    #[test]
    fn test_process_no_tasks() {
        let input = HookInput {
            session_id: Some("nonexistent-session".to_string()),
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    /// FS that reports a caller-supplied home dir so tests can isolate `~/.claude/`.
    struct ScopedHomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for ScopedHomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn replace_file_atomic(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            self.write(p, c)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            _: &Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

    /// Regression: `find_active_task_dir` must NOT fall back to the most
    /// recently modified dir in `~/.claude/tasks/`.
    #[test]
    fn test_find_active_task_dir_no_cross_project_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let tasks_root = home.join(".claude").join("tasks");

        let target_session = "target-session-uuid";
        let other_session = "other-session-uuid";

        let target_dir = tasks_root.join(target_session);
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("1.json"), "{}").unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        let other_dir = tasks_root.join(other_session);
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(other_dir.join("1.json"), "{}").unwrap();

        let fs = ScopedHomeFs { home };

        let found = find_active_task_dir(&fs, target_session).unwrap();
        assert_eq!(found, target_dir);

        let missing = find_active_task_dir(&fs, "no-such-session");
        assert!(
            missing.is_none(),
            "must not fall back to other sessions' dirs"
        );
    }

    #[test]
    fn test_find_active_task_dir_missing_tasks_root() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        assert!(find_active_task_dir(&fs, "any-session").is_none());
    }

    #[test]
    fn test_find_active_task_dir_empty_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let session_dir = home.join(".claude").join("tasks").join("session-x");
        std::fs::create_dir_all(&session_dir).unwrap();
        let fs = ScopedHomeFs { home };
        assert!(find_active_task_dir(&fs, "session-x").is_none());
    }

    #[test]
    fn test_write_project_tasks_md_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;
        let body = "auto body\n";
        let changed = write_project_tasks_md(&fs, root, body).unwrap();
        assert!(changed);
        let written = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert!(written.contains(MARKER_START));
        assert!(written.contains("auto body"));
        assert!(written.contains(MARKER_END));
    }

    #[test]
    fn test_write_project_tasks_md_skips_when_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;
        let body = "auto body\n";
        let first = write_project_tasks_md(&fs, root, body).unwrap();
        assert!(first);
        let second = write_project_tasks_md(&fs, root, body).unwrap();
        assert!(!second, "writing the same body twice must be a no-op");
    }

    #[test]
    fn test_encode_project_key_windows_path() {
        assert_eq!(
            encode_project_key("C:\\Users\\operator\\Documents\\GitHub\\sentinel"),
            "C--Users-operator-Documents-GitHub-sentinel"
        );
    }

    #[test]
    fn test_encode_project_key_unix_path() {
        assert_eq!(
            encode_project_key("/Users/operator/projects/firefly"),
            "-Users-operator-projects-firefly"
        );
    }

    #[test]
    fn test_write_memory_summary_skips_when_projects_dir_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let fs = ScopedHomeFs {
            home: home.to_path_buf(),
        };
        let repo_root = home.join("some_project");
        std::fs::create_dir_all(&repo_root).unwrap();
        let tasks = vec![Task {
            id: "1".into(),
            subject: "do thing".into(),
            description: String::new(),
            active_form: None,
            owner: None,
            status: "pending".into(),
            blocks: vec![],
            blocked_by: vec![],
            checklist: vec![],
            metadata: None,
        }];
        let wrote = write_memory_summary(&fs, &repo_root, &tasks).unwrap();
        assert!(
            !wrote,
            "should skip when ~/.claude/projects/<key> doesn't exist"
        );
    }

    #[test]
    fn test_write_memory_summary_creates_files() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let repo_root = home.join("myrepo");
        std::fs::create_dir_all(&repo_root).unwrap();

        // Create the matching projects dir.
        let key = encode_project_key(&repo_root.to_string_lossy());
        let project_dir = home.join(".claude").join("projects").join(&key);
        std::fs::create_dir_all(&project_dir).unwrap();

        let fs = ScopedHomeFs {
            home: home.to_path_buf(),
        };
        let tasks = vec![
            Task {
                id: "1".into(),
                subject: "alpha".into(),
                description: String::new(),
                active_form: None,
                owner: None,
                status: "in_progress".into(),
                blocks: vec![],
                blocked_by: vec![],
                checklist: vec![],
                metadata: None,
            },
            Task {
                id: "2".into(),
                subject: "beta".into(),
                description: String::new(),
                active_form: None,
                owner: None,
                status: "pending".into(),
                blocks: vec![],
                blocked_by: vec!["1".into()],
                checklist: vec![],
                metadata: None,
            },
        ];
        let wrote = write_memory_summary(&fs, &repo_root, &tasks).unwrap();
        assert!(wrote);

        let memory_file = project_dir.join("memory").join("project_tasks.md");
        let body = std::fs::read_to_string(&memory_file).unwrap();
        assert!(body.contains("type: project"));
        assert!(body.contains("source: auto"));
        assert!(body.contains("**#1** alpha"));
        assert!(body.contains("**#2** beta"));
        assert!(body.contains("blocked by 1"));

        let index = std::fs::read_to_string(project_dir.join("memory").join("MEMORY.md")).unwrap();
        assert!(index.contains("](project_tasks.md)"));
        assert!(index.contains("Tasks for myrepo"));
    }

    #[test]
    fn test_write_memory_summary_idempotent_index_line() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let repo_root = home.join("repo2");
        std::fs::create_dir_all(&repo_root).unwrap();
        let key = encode_project_key(&repo_root.to_string_lossy());
        let project_dir = home.join(".claude").join("projects").join(&key);
        let memory_dir = project_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        // Pre-existing MEMORY.md with one prior tasks line + an unrelated entry.
        std::fs::write(
            memory_dir.join("MEMORY.md"),
            "- [Other](other.md) — keep me\n\
             - [Tasks for old-name](project_tasks.md) — 99 open, 99 in progress\n",
        )
        .unwrap();

        let fs = ScopedHomeFs {
            home: home.to_path_buf(),
        };
        let tasks = vec![Task {
            id: "1".into(),
            subject: "x".into(),
            description: String::new(),
            active_form: None,
            owner: None,
            status: "pending".into(),
            blocks: vec![],
            blocked_by: vec![],
            checklist: vec![],
            metadata: None,
        }];
        write_memory_summary(&fs, &repo_root, &tasks).unwrap();

        let index = std::fs::read_to_string(memory_dir.join("MEMORY.md")).unwrap();
        // Old line replaced — only one Tasks-for line present.
        let count = index.matches("](project_tasks.md)").count();
        assert_eq!(
            count, 1,
            "duplicate Tasks-for index lines must be collapsed"
        );
        // Unrelated line preserved.
        assert!(index.contains("[Other](other.md)"));
        // New project name reflected.
        assert!(index.contains("Tasks for repo2"));
    }

    #[test]
    fn test_write_project_tasks_md_preserves_existing_non_block() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;
        let path = root.join("tasks.md");
        std::fs::write(&path, "# My Roadmap\n\nHand-written stuff.\n").unwrap();
        write_project_tasks_md(&fs, root, "auto body\n").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# My Roadmap"));
        assert!(written.contains("Hand-written stuff."));
        assert!(written.contains("auto body"));
    }

    /// Tests in this module touch the process-global env var
    /// `SHRINK_GUARD_FORCE_ENV`. Rust runs tests in parallel within a
    /// single process, so they MUST serialize their env-var access via
    /// this mutex or one test's mutation will race another test's read.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Render a synthetic auto-block body with `n_open` open + `n_done`
    /// completed tasks, matching the format `render_auto_block_body` emits.
    fn synth_body(n_open: usize, n_done: usize) -> String {
        let mut s = String::from("\n# Tasks — testproj\n\n");
        if n_open > 0 {
            s.push_str("## Open\n\n");
            for i in 1..=n_open {
                s.push_str(&format!("### [ ] {i}. Task {i}\n- **Status:** pending\n\n"));
            }
        }
        if n_done > 0 {
            s.push_str("## Completed\n\n");
            for i in (n_open + 1)..=(n_open + n_done) {
                s.push_str(&format!("- [x] **{i}. Done task {i}**\n"));
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn test_count_block_tasks_counts_open_and_completed() {
        let body = synth_body(3, 2);
        assert_eq!(count_block_tasks(&body), 5);
    }

    #[test]
    fn test_count_block_tasks_empty_body() {
        assert_eq!(count_block_tasks(""), 0);
        assert_eq!(count_block_tasks("# Tasks\n\nNo entries.\n"), 0);
    }

    #[test]
    fn test_extract_existing_block_returns_inner_body() {
        let content = format!(
            "# Roadmap\n\n{MARKER_START}\n# Tasks\n## Open\n### [ ] 1. X\n{MARKER_END}\nTail.\n"
        );
        let block = extract_existing_block(&content).expect("must extract");
        assert!(block.contains("# Tasks"));
        assert!(block.contains("### [ ] 1. X"));
        assert!(!block.contains("Tail."));
    }

    #[test]
    fn test_extract_existing_block_none_when_no_markers() {
        let content = "# Roadmap\n\nNo markers here.\n";
        assert!(extract_existing_block(content).is_none());
    }

    #[test]
    fn test_shrink_guard_refuses_to_wipe_large_existing() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;

        // Seed tasks.md with a large block (80 tasks).
        let big_body = synth_body(60, 20);
        write_project_tasks_md(&fs, root, &big_body).unwrap();
        let before = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&before), 80);

        // Attempt to replace with a 5-task block — should be refused.
        let tiny_body = synth_body(5, 0);
        let wrote = write_project_tasks_md(&fs, root, &tiny_body).unwrap();
        assert!(!wrote, "shrink guard must refuse the wipe");

        // Original block preserved.
        let after = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&after), 80);
    }

    #[test]
    fn test_shrink_guard_allows_routine_churn() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;

        // Seed with 20 tasks; replace with 15 — that's 75%, above the
        // 50% guard floor → must write.
        let initial = synth_body(15, 5);
        write_project_tasks_md(&fs, root, &initial).unwrap();
        let updated = synth_body(10, 5);
        let wrote = write_project_tasks_md(&fs, root, &updated).unwrap();
        assert!(wrote, "75% retention must not trip the guard");

        let after = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&after), 15);
    }

    #[test]
    fn test_shrink_guard_off_for_small_existing() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;

        // 8 tasks → 2 tasks. 8 is below SHRINK_GUARD_MIN_EXISTING (10),
        // so the guard never fires. Small projects can shrink freely.
        let initial = synth_body(8, 0);
        write_project_tasks_md(&fs, root, &initial).unwrap();
        let shrunk = synth_body(2, 0);
        let wrote = write_project_tasks_md(&fs, root, &shrunk).unwrap();
        assert!(
            wrote,
            "shrink guard must not fire for small existing blocks"
        );

        let after = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&after), 2);
    }

    #[test]
    fn test_shrink_guard_force_env_overrides() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;

        let big = synth_body(50, 10);
        write_project_tasks_md(&fs, root, &big).unwrap();

        // Set the force env and confirm the wipe goes through.
        std::env::set_var(SHRINK_GUARD_FORCE_ENV, "1");
        let tiny = synth_body(2, 0);
        let wrote = write_project_tasks_md(&fs, root, &tiny).unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        assert!(wrote, "force env var must override the guard");

        let after = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&after), 2);
    }

    #[test]
    fn test_shrink_guard_allows_initial_write() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;

        // No existing tasks.md → the guard has no prior block to compare
        // to; first-time writes always succeed regardless of size.
        let body = synth_body(2, 0);
        let wrote = write_project_tasks_md(&fs, root, &body).unwrap();
        assert!(wrote);
        let after = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&after), 2);
    }

    #[test]
    fn test_shrink_guard_allows_growth() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(SHRINK_GUARD_FORCE_ENV);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fs = TestFs;

        let initial = synth_body(20, 0);
        write_project_tasks_md(&fs, root, &initial).unwrap();
        let grown = synth_body(25, 5);
        let wrote = write_project_tasks_md(&fs, root, &grown).unwrap();
        assert!(wrote, "growth must always write through");
        let after = std::fs::read_to_string(root.join("tasks.md")).unwrap();
        assert_eq!(count_block_tasks(&after), 30);
    }

    // ───────────── normalize_task / decoration stripping ─────────────

    fn bare_task(id: &str, subject: &str, status: &str) -> Task {
        Task {
            id: id.to_string(),
            subject: subject.to_string(),
            description: String::new(),
            active_form: None,
            owner: None,
            status: status.to_string(),
            blocks: vec![],
            blocked_by: vec![],
            checklist: vec![],
            metadata: None,
        }
    }

    #[test]
    fn strip_subject_decoration_cases() {
        assert_eq!(
            strip_subject_decoration("🔄 🔴 1 [P0] — Fix memory-capture gate"),
            "Fix memory-capture gate"
        );
        assert_eq!(strip_subject_decoration("✅ Ship the thing"), "Ship the thing");
        assert_eq!(strip_subject_decoration("[P1] Do the work"), "Do the work");
        assert_eq!(strip_subject_decoration("2 - build it"), "build it");
        // Clean subject is untouched.
        assert_eq!(
            strip_subject_decoration("Restore mcpServers registrations"),
            "Restore mcpServers registrations"
        );
        // A glyph mid-subject is preserved (only leading decoration is stripped).
        assert_eq!(strip_subject_decoration("Add 🔴 marker to UI"), "Add 🔴 marker to UI");
        // Idempotent.
        let once = strip_subject_decoration("🔴 [P0] — X");
        assert_eq!(strip_subject_decoration(once), once);
    }

    #[test]
    fn normalize_task_cleans_subject_and_keeps_authoritative_status() {
        // Glyph says in-progress (🔄) but the field explicitly says pending —
        // the field wins; the subject is cleaned; P0 backfills from 🔴/[P0].
        let t = normalize_task(bare_task(
            "1",
            "🔄 🔴 1 [P0] — Fix memory-capture dual-judge gate",
            "pending",
        ));
        assert_eq!(t.subject, "Fix memory-capture dual-judge gate");
        assert_eq!(t.status, "pending", "explicit status field is authoritative");
        assert_eq!(
            t.metadata
                .as_ref()
                .and_then(|m| m.get("priority"))
                .and_then(|p| p.as_str()),
            Some("P0"),
            "priority backfilled from decoration when field absent"
        );
    }

    #[test]
    fn normalize_task_infers_status_only_when_field_blank() {
        // Empty status field → glyph fills it.
        let t = normalize_task(bare_task("2", "✅ Done thing", ""));
        assert_eq!(t.status, "completed");
        assert_eq!(t.subject, "Done thing");
    }

    #[test]
    fn normalize_task_priority_field_beats_glyph() {
        let mut base = bare_task("3", "🟢 [P3] — Low prio task", "in_progress");
        base.metadata = Some(serde_json::json!({ "priority": "P1" }));
        let t = normalize_task(base);
        assert_eq!(t.subject, "Low prio task");
        assert_eq!(
            t.metadata
                .as_ref()
                .and_then(|m| m.get("priority"))
                .and_then(|p| p.as_str()),
            Some("P1"),
            "explicit priority field is not overridden by the 🟢/[P3] glyph"
        );
    }

    #[test]
    fn normalize_task_clean_subject_is_noop() {
        let t = normalize_task(bare_task("4", "Restore mcpServers registrations", "pending"));
        assert_eq!(t.subject, "Restore mcpServers registrations");
        assert_eq!(t.status, "pending");
        assert!(t.metadata.is_none(), "no decoration → no priority backfill");
    }

    #[test]
    fn truncate_persistent_tasks_is_noop_when_no_prior_snapshot() {
        // A project that never had tasks: no tasks.json → truncate writes nothing.
        let tmp = tempfile::tempdir().unwrap();
        let fs = HomeFs {
            home: tmp.path().to_path_buf(),
        };
        let git = NoGit;
        let proj_hash = "deadbeef";
        truncate_persistent_tasks(&fs, &git, "/nonexistent/cwd", proj_hash, "sess-1").unwrap();
        let p = tmp
            .path()
            .join(".claude/sentinel/persistent-tasks")
            .join(proj_hash)
            .join("tasks.json");
        assert!(!p.exists(), "no snapshot must be created for a task-less project");
    }

    #[test]
    fn truncate_persistent_tasks_clears_stale_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = HomeFs {
            home: tmp.path().to_path_buf(),
        };
        let git = NoGit;
        let proj_hash = "cafef00d";
        let dir = tmp
            .path()
            .join(".claude/sentinel/persistent-tasks")
            .join(proj_hash);
        std::fs::create_dir_all(&dir).unwrap();
        // Seed a stale non-empty snapshot.
        std::fs::write(
            dir.join("tasks.json"),
            r#"[{"id":"1","subject":"ghost","status":"in_progress"}]"#,
        )
        .unwrap();
        truncate_persistent_tasks(&fs, &git, "/some/cwd", proj_hash, "sess-2").unwrap();
        let after = std::fs::read_to_string(dir.join("tasks.json")).unwrap();
        assert_eq!(after.trim(), "[]", "stale snapshot must be truncated to []");
        let meta: PersistMeta =
            serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta.task_count, 0);
        assert_eq!(meta.incomplete_count, 0);
    }

    /// Real-FS test double rooted at a fixed home dir (so `home_dir()` points
    /// at a tempdir). All other ops delegate to `std::fs`.
    struct HomeFs {
        home: PathBuf,
    }
    impl FileSystemPort for HomeFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(
            &self,
            p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn replace_file_atomic(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            self.write(p, c)
        }
        fn create_dir_all(
            &self,
            p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(
            &self,
            p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::read_dir(p)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect())
        }
        fn exists(&self, p: &Path) -> bool {
            p.exists()
        }
        fn is_dir(&self, p: &Path) -> bool {
            p.is_dir()
        }
        fn metadata(
            &self,
            p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(
            &self,
            p: &Path,
            c: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            f.write_all(c)?;
            Ok(())
        }
    }

    /// Git port that reports "not a repo" for everything — truncate then skips
    /// the tasks.md write path and only touches the JSON snapshot + meta.
    struct NoGit;
    impl GitStatusPort for NoGit {
        fn has_uncommitted_changes(&self, _: &str) -> Result<bool, GitError> {
            Ok(false)
        }
        fn changed_files(&self, _: &str) -> Result<Vec<String>, GitError> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> Result<String, GitError> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> Result<bool, GitError> {
            Ok(false)
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn rev_list_count_range(&self, _: &str, _: &str) -> Option<u32> {
            None
        }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> {
            None
        }
        fn merged_local_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn merged_remote_branches(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn head_sha(&self, _: &str) -> Option<String> {
            None
        }
    }
}
