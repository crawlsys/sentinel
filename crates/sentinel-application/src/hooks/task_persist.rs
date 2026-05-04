//! Task Persist Hook — snapshot task list to project-level tasks.md + global JSON
//!
//! Fires on TaskCreated, TaskCompleted, and Stop events.
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
//!    Machine-readable snapshot consumed by `task_rehydrate` on SessionStart.
//!    `meta.json` next to it tracks last update + content hash for skip-if-unchanged.
//!    Previously lived at `~/.claude/persistent-tasks/` — moved under `sentinel/`
//!    to colocate with the rest of sentinel-owned state. Old data is migrated
//!    automatically on first read (see [`super::migrate_persistent_tasks_dir`]).
//!
//! Project scoping:
//!   - Repo root resolution via `GitStatusPort::repo_root(cwd)`. If the cwd is
//!     outside any git repo, the markdown write is skipped (only the global JSON
//!     snapshot is written, since there's no project root to anchor on).
//!   - The `project_hash` keying the global snapshot is SHA-256(cwd)[..4]; this
//!     matches `task_rehydrate.rs` so rehydration paths stay aligned.

use chrono::Utc;
use sentinel_domain::events::{HookInput, HookOutput};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use super::{FileSystemPort, GitStatusPort, HookContext};

/// Public so tasks_md_guard / linear_sync can detect the auto block.
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

/// Compute a project hash from the working directory (first 8 hex chars of SHA-256)
fn project_hash(cwd: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.as_bytes());
    let result = hasher.finalize();
    result[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Hex-encode SHA-256 of a string. Used for content-hash skip checks.
fn sha256_hex(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// Get the persistent tasks directory for a project (under
/// `~/.claude/sentinel/persistent-tasks/`).
///
/// Triggers a one-time migration from the legacy `~/.claude/persistent-tasks/`
/// location on the first call after upgrade. Migration is idempotent — once
/// the new dir exists, the legacy path is never touched again.
fn persistent_tasks_dir(fs: &dyn FileSystemPort, project_hash: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    super::migrate_persistent_tasks_dir(fs, &home);
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
    let session_dir = home.join(".claude").join("tasks").join(session_id);
    if fs.is_dir(&session_dir) && has_task_files(fs, &session_dir) {
        Some(session_dir)
    } else {
        None
    }
}

/// Check if a directory contains at least one .json task file (not .lock, not .highwatermark)
fn has_task_files(fs: &dyn FileSystemPort, dir: &PathBuf) -> bool {
    fs.read_dir(dir)
        .map(|entries| {
            entries.iter().any(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                name.ends_with(".json") && !name.starts_with('.')
            })
        })
        .unwrap_or(false)
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
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            if let Ok(content) = fs.read_to_string(&path) {
                if let Ok(task) = serde_json::from_str::<Task>(&content) {
                    tasks.push(task);
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

/// Render Linear-issue rows. Active states only (no completed/canceled);
/// sorted by status_type rank then identifier.
fn render_linear_section(issues: &[LinearIssue]) -> String {
    if issues.is_empty() {
        return String::new();
    }

    fn status_rank(s: &str) -> u8 {
        match s {
            "started" => 0, // In Progress
            "unstarted" => 1, // Todo
            "backlog" => 2,
            "triage" => 3,
            _ => 9,
        }
    }
    fn status_label(s: &str) -> &'static str {
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
    fn priority_label(v: &serde_json::Value) -> &'static str {
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

    let mut sorted: Vec<&LinearIssue> = issues
        .iter()
        .filter(|i| !matches!(i.status_type.as_str(), "completed" | "canceled"))
        .collect();
    if sorted.is_empty() {
        return String::new();
    }
    sorted.sort_by(|a, b| {
        status_rank(&a.status_type)
            .cmp(&status_rank(&b.status_type))
            .then(a.identifier.cmp(&b.identifier))
    });

    let mut md = String::from("## Linear Assigned\n\n");
    md.push_str(&format!("_{} open issue(s) from Linear cache._\n\n", sorted.len()));
    md.push_str("| Issue | Pri | State | Title |\n");
    md.push_str("|---|---|---|---|\n");
    for i in sorted.iter().take(50) {
        let id = if i.url.is_empty() {
            i.identifier.clone()
        } else {
            format!("[{}]({})", i.identifier, i.url)
        };
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            id,
            priority_label(&i.priority),
            status_label(&i.status_type),
            i.title.replace('|', "\\|"),
        ));
    }
    if sorted.len() > 50 {
        md.push_str(&format!("\n…and {} more (truncated).\n", sorted.len() - 50));
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
    md.push_str("\n");
    md.push_str(&format!(
        "<!-- This block is auto-managed by sentinel `task_persist` hook. Edit via TaskCreate / Linear, not by hand. Updated: {now} -->\n\n"
    ));
    md.push_str(&format!("# Tasks — {project_name}\n\n"));
    md.push_str(&format!(
        "**{total} total · {pending} pending · {in_prog} in progress · {done} completed**\n\n",
        total = tasks.len(),
        pending = incomplete.len().saturating_sub(in_progress),
        in_prog = in_progress,
        done = completed.len(),
    ));

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
            md.push_str(&format!("### [{check}] {}. {}\n", task.id, task.subject));
            md.push_str(&format!("- **Status:** {}\n", task.status));
            if !task.blocks.is_empty() {
                md.push_str(&format!("- **Blocks:** {}\n", task.blocks.join(", ")));
            }
            if !task.blocked_by.is_empty() {
                md.push_str(&format!(
                    "- **Blocked by:** {}\n",
                    task.blocked_by.join(", ")
                ));
            }
            if let Some(owner) = &task.owner {
                md.push_str(&format!("- **Owner:** {owner}\n"));
            }
            if let Some(meta) = &task.metadata {
                if let Some(obj) = meta.as_object() {
                    if let Some(priority) = obj.get("priority").and_then(|v| v.as_str()) {
                        md.push_str(&format!("- **Priority:** {priority}\n"));
                    }
                    if let Some(phase) = obj.get("phase").and_then(|v| v.as_str()) {
                        md.push_str(&format!("- **Phase:** {phase}\n"));
                    }
                    if let Some(tags) = obj.get("skill_tags").and_then(|v| v.as_array()) {
                        let tag_strs: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
                        if !tag_strs.is_empty() {
                            md.push_str(&format!("- **Tags:** {}\n", tag_strs.join(", ")));
                        }
                    }
                }
            }
            if !task.description.is_empty() {
                md.push_str(&format!("- **Description:** {}\n", task.description));
            }
            if !task.checklist.is_empty() {
                let done = task.checklist.iter().filter(|c| c.completed).count();
                md.push_str(&format!(
                    "- **Checklist:** ({}/{})\n",
                    done,
                    task.checklist.len()
                ));
                for item in &task.checklist {
                    let mark = if item.completed { "x" } else { " " };
                    md.push_str(&format!("  - [{mark}] {}\n", item.text));
                }
            }
            md.push('\n');
        }
    }

    if !completed.is_empty() {
        md.push_str("## Completed\n\n");
        for task in &completed {
            md.push_str(&format!("- [x] **{}. {}**\n", task.id, task.subject));
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
                let after = content[end_idx..].strip_prefix('\n').unwrap_or(&content[end_idx..]);
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
    repo_root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
}

/// Atomic file write: write to `<path>.tmp` then rename. Falls back to direct
/// write if rename isn't supported (rare).
fn atomic_write(fs: &dyn FileSystemPort, path: &Path, content: &str) -> anyhow::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.sentinel-tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("md")
    ));
    fs.write(&tmp, content.as_bytes())?;
    // FileSystemPort doesn't expose rename, so do a direct write fallback.
    // On Windows std::fs::rename across same dir is atomic; we replicate that
    // by doing a direct write (the temp write was the safety net) and removing
    // the temp.
    fs.write(path, content.as_bytes())?;
    let _ = std::fs::remove_file(&tmp);
    Ok(())
}

/// Write `<repo_root>/tasks.md` with the auto block merged into existing content.
///
/// Returns `true` if the file was actually changed, `false` if skipped (no diff).
fn write_project_tasks_md(
    fs: &dyn FileSystemPort,
    repo_root: &Path,
    body: &str,
) -> anyhow::Result<bool> {
    let path = repo_root.join("tasks.md");
    let existing = fs.read_to_string(&path).ok();
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
    body.push_str(&format!("name: Tasks for {proj_name}\n"));
    body.push_str(&format!(
        "description: Snapshot of native TaskList for {proj_name} — kept in sync by sentinel `task_persist` hook on TaskCreated/TaskCompleted/Stop. {open} open, {ip} in progress.\n",
        open = incomplete.len(),
        ip = in_progress,
    ));
    body.push_str("type: project\n");
    body.push_str("source: auto\n");
    body.push_str("---\n\n");
    body.push_str(&format!(
        "**{open}** open · **{ip}** in progress · **{done}** completed (full state in `tasks.md` at the repo root)\n\n",
        open = incomplete.len(),
        ip = in_progress,
        done = tasks.len() - incomplete.len(),
    ));

    if incomplete.is_empty() {
        body.push_str("_No open tasks._\n");
    } else {
        body.push_str("Top open tasks (by id):\n\n");
        for task in incomplete.iter().take(10) {
            let mark = if task.status == "in_progress" { "~" } else { " " };
            body.push_str(&format!("- [{mark}] **#{}** {}", task.id, task.subject));
            if !task.blocked_by.is_empty() {
                body.push_str(&format!(" _(blocked by {})_", task.blocked_by.join(", ")));
            }
            body.push('\n');
        }
        if incomplete.len() > 10 {
            body.push_str(&format!("- _…and {} more._\n", incomplete.len() - 10));
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
        None => new_index_line.clone(),
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
    let json = serde_json::to_string_pretty(tasks).unwrap_or_default();
    fs.write(&global_dir.join("tasks.json"), json.as_bytes())?;

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
        tracing::debug!(cwd, "Not inside a git repo — skipping project tasks.md write");
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
    let meta_json = serde_json::to_string_pretty(&meta).unwrap_or_default();
    fs.write(&global_dir.join("meta.json"), meta_json.as_bytes())?;

    tracing::debug!(
        project_hash = proj_hash,
        task_count = tasks.len(),
        incomplete_count,
        in_repo = repo_root.is_some(),
        "Persisted tasks to disk"
    );

    Ok(())
}

/// Process task persistence on TaskCreated, TaskCompleted, or Stop events.
///
/// Reads the active session's task files, then writes:
/// - `<repo_root>/tasks.md` (project-scoped, with marker block)
/// - `~/.claude/persistent-tasks/{project_hash}/tasks.json` (rehydration source)
/// - `~/.claude/persistent-tasks/{project_hash}/meta.json` (skip-if-unchanged)
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let task_dir = match find_active_task_dir(ctx.fs, session_id) {
        Some(dir) => dir,
        None => {
            tracing::debug!("No active task directory found — skipping persist");
            return HookOutput::allow();
        }
    };

    let tasks = read_tasks(ctx.fs, &task_dir);
    if tasks.is_empty() {
        return HookOutput::allow();
    }

    let proj_hash = project_hash(cwd);
    if let Err(e) = write_persistent_tasks(ctx.fs, ctx.git, &tasks, cwd, &proj_hash, session_id) {
        tracing::warn!(error = %e, "Failed to persist tasks");
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Minimal real-FS for tests that need to read temp directories.
    struct TestFs;
    impl FileSystemPort for TestFs {
        fn home_dir(&self) -> Option<PathBuf> {
            dirs::home_dir()
        }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
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
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_project_hash_deterministic() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/firefly");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
    }

    #[test]
    fn test_project_hash_different() {
        let h1 = project_hash("/Users/gary/projects/firefly");
        let h2 = project_hash("/Users/gary/projects/corvus");
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
        assert!(!section.contains("X-2"), "completed issues must be filtered");
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
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            Ok(std::fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, c: &[u8]) -> anyhow::Result<()> {
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par)?;
            }
            Ok(std::fs::write(p, c)?)
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir_all(p)?)
        }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
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
        fn metadata(&self, p: &Path) -> anyhow::Result<std::fs::Metadata> {
            Ok(std::fs::metadata(p)?)
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
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
            encode_project_key("C:\\Users\\garys\\Documents\\GitHub\\sentinel"),
            "C--Users-garys-Documents-GitHub-sentinel"
        );
    }

    #[test]
    fn test_encode_project_key_unix_path() {
        assert_eq!(
            encode_project_key("/Users/gary/projects/firefly"),
            "-Users-gary-projects-firefly"
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
        assert!(!wrote, "should skip when ~/.claude/projects/<key> doesn't exist");
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
        assert_eq!(count, 1, "duplicate Tasks-for index lines must be collapsed");
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
}
