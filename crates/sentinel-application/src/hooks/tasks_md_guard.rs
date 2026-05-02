//! Tasks.md Auto-Block Guard
//!
//! PreToolUse gate that blocks `Edit` / `Write` tool calls which would mutate
//! content **inside** the `<!-- SENTINEL:TASKS:START --> ... <!-- SENTINEL:TASKS:END -->`
//! marker block in any project's `tasks.md`.
//!
//! Why: the auto block is owned by the `task_persist` hook (driven by
//! `TaskCreate` / `TaskUpdate` and Linear sync). Hand-edits inside it get
//! clobbered on the next persist write, so silently-allowed edits create the
//! illusion of changes that vanish a second later. Better to fail fast and
//! tell the agent / user to use `TaskCreate` (or edit outside the markers).
//!
//! What's allowed:
//!   - Edits to a `tasks.md` file with no markers (untouched user file).
//!   - Edits whose `old_string` is fully outside the marker block.
//!   - Writes that re-emit the file with the marker block byte-identical
//!     to what's already on disk (the `task_persist` hook itself does this).
//!     Also applies to a fresh write that *adds* a well-formed block where
//!     none existed.
//!   - Any path that isn't a `tasks.md` at the root of a git repo.
//!
//! What's blocked:
//!   - `Edit` whose `old_string` overlaps the marker block range.
//!   - `Write` whose new content changes the bytes between the markers
//!     (vs. what's currently on disk between the markers).

use sentinel_domain::events::{HookInput, HookOutput};
use std::path::Path;

use super::task_persist::{MARKER_END, MARKER_START};
use super::{GitStatusPort, HookContext};

/// Tools we care about. Bash / other tools can do whatever; only the direct
/// file-mutating tools risk silently-overwritten edits.
const GUARDED_TOOLS: &[&str] = &["Edit", "Write"];

/// Pull `file_path` from either Claude Code 2.1.89+ top-level field or the
/// older `tool_input.file_path` location.
fn file_path_from_input(input: &HookInput) -> Option<String> {
    if let Some(p) = &input.file_path {
        if !p.is_empty() {
            return Some(p.clone());
        }
    }
    input
        .tool_input
        .as_ref()
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

/// Pull a string field out of `tool_input`. Returns `""` when missing.
fn tool_field<'a>(input: &'a HookInput, field: &str) -> &'a str {
    input
        .tool_input
        .as_ref()
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// True when `file_path` is exactly the project's root `tasks.md`.
///
/// We anchor on the git repo root (via `GitStatusPort::repo_root`) so that
/// `tasks.md` files in subdirectories or unrelated projects are NOT guarded —
/// only the file that `task_persist` actually writes.
fn is_project_tasks_md(git: &dyn GitStatusPort, file_path: &str) -> bool {
    let p = Path::new(file_path);
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !name.eq_ignore_ascii_case("tasks.md") {
        return false;
    }
    let parent = match p.parent() {
        Some(par) => par.to_string_lossy().to_string(),
        None => return false,
    };
    let Some(repo_root) = git.repo_root(&parent) else {
        return false;
    };
    // Compare canonical-ish: trim trailing slashes, case-insensitive on Windows.
    let parent_norm = parent.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    let root_norm = repo_root.trim_end_matches(['\\', '/']).to_ascii_lowercase();
    parent_norm == root_norm
}

/// Extract the byte range `[start, end)` of the auto block (markers inclusive)
/// from a file body. Returns `None` if either marker is missing or out of order.
fn marker_range(content: &str) -> Option<(usize, usize)> {
    let s = content.find(MARKER_START)?;
    let e = content.find(MARKER_END)?;
    if e <= s {
        return None;
    }
    Some((s, e + MARKER_END.len()))
}

/// Extract the bytes between the markers (markers excluded). Returns `""`
/// when there is no block.
fn block_inner<'a>(content: &'a str) -> &'a str {
    let Some((s, e)) = marker_range(content) else {
        return "";
    };
    let inner_start = s + MARKER_START.len();
    let inner_end = e - MARKER_END.len();
    if inner_end <= inner_start {
        return "";
    }
    &content[inner_start..inner_end]
}

/// Decide whether an `Edit` call would mutate the auto block.
///
/// Strategy: if `old_string` is empty (which Claude Code's Edit doesn't accept,
/// but we're conservative) or doesn't appear in the existing file, let the
/// upstream tool handle the error. Otherwise, find the position of `old_string`
/// in the existing content and check whether `[pos, pos + old_string.len())`
/// overlaps the marker range.
fn edit_overlaps_block(existing: &str, old_string: &str) -> bool {
    let Some((s, e)) = marker_range(existing) else {
        return false;
    };
    if old_string.is_empty() {
        return false;
    }
    let Some(pos) = existing.find(old_string) else {
        return false;
    };
    let edit_start = pos;
    let edit_end = pos + old_string.len();
    // Overlap = NOT (edit ends before block) AND NOT (edit starts after block).
    !(edit_end <= s || edit_start >= e)
}

/// Decide whether a `Write` call would change the auto block.
///
/// Compares the bytes-between-markers in the existing file vs. the proposed
/// content. Both have the same auto-block contents → allow. Different → block.
/// New file (or no existing block) → allow only if the new content has a
/// well-formed (non-empty markers, end-after-start) block.
fn write_changes_block(existing: Option<&str>, new_content: &str) -> bool {
    let existing_inner = existing.map(block_inner).unwrap_or("");
    let new_inner = block_inner(new_content);

    // No existing block → any well-formed new block is allowed (creating the
    // file fresh is a normal first-write case).
    if existing_inner.is_empty() {
        return false;
    }

    existing_inner != new_inner
}

/// Build the block message returned to the agent.
fn block_message(file_path: &str, action: &str) -> String {
    format!(
        "tasks.md auto block is owned by the sentinel `task_persist` hook \
         (driven by TaskCreate / Linear sync). {action} would change content \
         inside the `<!-- SENTINEL:TASKS:START --> … <!-- SENTINEL:TASKS:END -->` \
         markers, which gets overwritten on the next task event.\n\
         \n\
         Path: {file_path}\n\
         \n\
         Fix:\n\
         - To add or modify a task, call `TaskCreate` / `TaskUpdate`. The \
           hook will re-render the block on the next event.\n\
         - To edit hand-written content, edit OUTSIDE the marker block — \
           anything before `SENTINEL:TASKS:START` or after `SENTINEL:TASKS:END` \
           is preserved verbatim.",
        action = action,
        file_path = file_path,
    )
}

/// Process a PreToolUse event. Returns `HookOutput::block(msg)` when the call
/// would mutate the auto block; `HookOutput::allow()` otherwise.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool_name = input.tool_name.as_deref().unwrap_or("");
    if !GUARDED_TOOLS.iter().any(|t| t.eq_ignore_ascii_case(tool_name)) {
        return HookOutput::allow();
    }

    let Some(file_path) = file_path_from_input(input) else {
        return HookOutput::allow();
    };

    if !is_project_tasks_md(ctx.git, &file_path) {
        return HookOutput::allow();
    }

    let existing = ctx.fs.read_to_string(Path::new(&file_path)).ok();

    match tool_name {
        t if t.eq_ignore_ascii_case("Edit") => {
            let Some(existing) = existing else {
                // Edit on a missing file → upstream will reject; stay out.
                return HookOutput::allow();
            };
            let old_string = tool_field(input, "old_string");
            if edit_overlaps_block(&existing, old_string) {
                return HookOutput::block(block_message(&file_path, "Edit"));
            }
            HookOutput::allow()
        }
        t if t.eq_ignore_ascii_case("Write") => {
            let new_content = tool_field(input, "content");
            if write_changes_block(existing.as_deref(), new_content) {
                return HookOutput::block(block_message(&file_path, "Write"));
            }
            HookOutput::allow()
        }
        _ => HookOutput::allow(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(body: &str) -> String {
        format!("{MARKER_START}\n{body}\n{MARKER_END}\n")
    }

    #[test]
    fn marker_range_none_without_markers() {
        assert!(marker_range("# Hello\nNo markers here\n").is_none());
    }

    #[test]
    fn marker_range_finds_block() {
        let b = block("auto");
        let r = marker_range(&b).unwrap();
        assert!(r.0 < r.1);
        assert_eq!(&b[r.0..r.0 + MARKER_START.len()], MARKER_START);
    }

    #[test]
    fn block_inner_strips_markers() {
        let b = block("auto-content");
        assert_eq!(block_inner(&b).trim(), "auto-content");
        assert_eq!(block_inner("no markers"), "");
    }

    #[test]
    fn edit_inside_block_overlaps() {
        let existing = block("auto-content");
        // old_string lives inside the markers
        assert!(edit_overlaps_block(&existing, "auto-content"));
    }

    #[test]
    fn edit_above_block_does_not_overlap() {
        let existing = format!("# Roadmap\n\nKeep me\n\n{}", block("auto"));
        assert!(!edit_overlaps_block(&existing, "Keep me"));
    }

    #[test]
    fn edit_below_block_does_not_overlap() {
        let existing = format!("{}\nFooter line\n", block("auto"));
        assert!(!edit_overlaps_block(&existing, "Footer line"));
    }

    #[test]
    fn edit_no_markers_does_not_overlap() {
        let existing = "# Hand-written tasks\n- foo\n- bar\n";
        assert!(!edit_overlaps_block(existing, "- foo"));
    }

    #[test]
    fn write_unchanged_block_allowed() {
        let existing = block("auto");
        let new_content = block("auto");
        assert!(!write_changes_block(Some(&existing), &new_content));
    }

    #[test]
    fn write_changed_block_blocked() {
        let existing = block("old auto");
        let new_content = block("new auto");
        assert!(write_changes_block(Some(&existing), &new_content));
    }

    #[test]
    fn write_no_existing_file_allowed() {
        let new_content = block("auto");
        assert!(!write_changes_block(None, &new_content));
    }

    #[test]
    fn write_existing_no_markers_allowed() {
        // Existing file has no block at all — first-write that *adds* the
        // block is allowed (this is what task_persist does on first run).
        let existing = "# Existing roadmap\n";
        let new_content = format!("{existing}\n{}", block("auto"));
        assert!(!write_changes_block(Some(existing), &new_content));
    }

    #[test]
    fn block_message_mentions_marker_names() {
        let msg = block_message("/repo/tasks.md", "Edit");
        assert!(msg.contains("SENTINEL:TASKS:START"));
        assert!(msg.contains("SENTINEL:TASKS:END"));
        assert!(msg.contains("/repo/tasks.md"));
        assert!(msg.contains("TaskCreate"));
    }
}
