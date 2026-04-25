//! Tool Usage Gate
//!
//! PreToolUse hook that blocks Edit/Write if required preconditions aren't met:
//! 1. Sequential thinking must have been used this session
//! 2. At least one task must have been created this session
//! 3. A plan must have been approved this session (ExitPlanMode/EnterPlanMode
//!    called, OR a recent `plans/*.md` exists)
//! 4. A task must be actively in_progress
//!
//! State is tracked via marker files in the temp directory, keyed by session ID.
//! Marker files are written by the PostToolUse dispatcher when it detects
//! the relevant tool calls.
//!
//! Plan-mode detection (primary): read the session transcript at
//! `input.transcript_path` and walk it to find the last `EnterPlanMode` or
//! `ExitPlanMode` `tool_use` entry. If `EnterPlanMode` appears after the
//! last `ExitPlanMode` (or there is no ExitPlanMode), the session is
//! currently in plan mode and check #3 is satisfied directly by the real
//! Claude Code 2.1.114 signal. This replaces the old `SENTINEL_AUTOPILOT`
//! env-var bypass — see `detect_plan_mode_from_transcript`.
//!
//! Autopilot fallback: `SENTINEL_AUTOPILOT=1` is retained only as a
//! last-resort escape hatch for the rare case where the hook fires with
//! no transcript path at all (e.g. malformed harness input). In normal
//! operation the transcript is authoritative and this env var is ignored.
//!
//! Plan-file fallback: when a session is resumed, `ExitPlanMode` may have
//! been called in a prior session and the transcript may not be available,
//! so no PLAN_MARKER exists. Detect this by scanning for recently-written
//! `*.md` files in any `plans/` directory between `{cwd}` and the
//! containing repo root. We walk upward from cwd until we find a `.git`
//! entry (file or directory — worktrees use a file) and check every
//! `plans/` dir we find along the way. This handles:
//!   - cwd is the repo root           → checks `{root}/plans/`
//!   - cwd is a git worktree          → `.git` file in worktree lands us at
//!                                       the worktree root, so plans authored
//!                                       at the main repo root are still
//!                                       reachable via the worktree's own
//!                                       `plans/` dir (Claude Code writes
//!                                       plans relative to cwd)
//!   - cwd is a nested subdirectory   → walks up checking each level

use sentinel_domain::events::{HookInput, HookOutput};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::hygiene_override::is_signed_override_active;
use super::{EnvPort, FileSystemPort};

/// Last-resort escape hatch for when the hook fires with no transcript
/// path at all. In normal 2.1.114 operation the transcript is the
/// authoritative signal and this env var is ignored.
fn is_autopilot(env: &dyn EnvPort) -> bool {
    env.var("SENTINEL_AUTOPILOT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Walk the transcript newest-to-oldest looking for the most recent
/// `EnterPlanMode` or `ExitPlanMode` `tool_use` entry. Returns `true` iff
/// the last one is `EnterPlanMode` — meaning the session is currently in
/// plan mode.
///
/// Claude Code 2.1.114 records plan-mode entry as an assistant tool_use
/// block with `name: "EnterPlanMode"` (real tool — binary handler `r7H` —
/// though omitted from `sdk-tools.d.ts`). Exit is a tool_use with
/// `name: "ExitPlanMode"`. Between those two calls the permission context
/// carries `mode: "plan"`.
///
/// We parse lines lazily from the end — the file is read fully into memory,
/// but JSON parsing short-circuits on the first plan-related `tool_use`
/// encountered going backwards, which is the current state. The inner block
/// iteration also runs in reverse so the latest `tool_use` within a single
/// assistant message wins when multiple plan signals appear in one message.
pub fn detect_plan_mode_from_transcript(transcript_path: &Path) -> bool {
    let content = match std::fs::read_to_string(transcript_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    for line in content.lines().rev() {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let Some(blocks) = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };

        for block in blocks.iter().rev() {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }
            match block.get("name").and_then(|v| v.as_str()) {
                Some("EnterPlanMode") => return true,
                Some("ExitPlanMode") => return false,
                _ => {}
            }
        }
    }

    false
}

/// How recent a plan file must be to count as "approved this session".
/// 7 days covers resumed sessions while still requiring a plan per week.
const PLAN_FILE_FRESH_WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Marker file prefix for sequential thinking usage.
const SEQUENTIAL_MARKER_PREFIX: &str = "claude-sequential-used-";

/// Marker file prefix for task creation.
const TASK_MARKER_PREFIX: &str = "claude-task-created-";

/// Marker file prefix for plan approval (ExitPlanMode was called).
const PLAN_MARKER_PREFIX: &str = "claude-plan-approved-";

/// Marker file prefix for active task (TaskUpdate set a task to in_progress).
const TASK_ACTIVE_PREFIX: &str = "claude-task-active-";

/// Check if a marker file exists for this session.
fn has_marker(fs: &dyn FileSystemPort, prefix: &str, session_id: &str) -> bool {
    let path = temp_marker_path(prefix, session_id);
    fs.exists(&path)
}

/// Max directory levels to walk up from cwd looking for plans/ dirs.
/// Prevents pathological runaway on unusual filesystems.
const MAX_WALK_UP_DEPTH: usize = 10;

/// True if `dir` contains a `plans/*.md` written inside the freshness window.
fn plans_dir_has_recent_md(fs: &dyn FileSystemPort, dir: &Path, now: SystemTime) -> bool {
    if !fs.is_dir(dir) {
        return false;
    }
    let entries = match fs.read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    entries.iter().any(|entry| {
        if entry.extension().and_then(|e| e.to_str()) != Some("md") {
            return false;
        }
        match fs.metadata(entry).and_then(|m| m.modified().map_err(Into::into)) {
            Ok(modified) => now
                .duration_since(modified)
                .map(|age| age <= PLAN_FILE_FRESH_WINDOW)
                .unwrap_or(false),
            Err(_) => false,
        }
    })
}

/// Walk up from cwd toward filesystem root, returning true if any `plans/`
/// directory along the way has a recently-written `*.md`. Stops at the first
/// directory containing a `.git` entry (marker of the repo boundary) — that
/// directory is still checked, but we don't ascend past it.
fn has_recent_plan_file(fs: &dyn FileSystemPort, cwd: Option<&str>, now: SystemTime) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let mut current: Option<&Path> = Some(Path::new(cwd));
    for _ in 0..MAX_WALK_UP_DEPTH {
        let Some(dir) = current else { break };
        if plans_dir_has_recent_md(fs, &dir.join("plans"), now) {
            return true;
        }
        // Stop once we've inspected the repo root (detected by `.git`).
        // `.git` can be either a directory (normal repo) or a file (worktree).
        if fs.exists(&dir.join(".git")) {
            return false;
        }
        current = dir.parent();
    }
    false
}

/// Build the temp-dir path for a marker file.
fn temp_marker_path(prefix: &str, session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}{session_id}"))
}

/// Write a marker file to record that a precondition has been met.
pub fn write_marker(fs: &dyn FileSystemPort, prefix: &str, session_id: &str) {
    let path = temp_marker_path(prefix, session_id);
    let _ = fs.write(&path, b"1");
}

/// Write the sequential-thinking marker for this session.
pub fn mark_sequential_thinking_used(fs: &dyn FileSystemPort, session_id: &str) {
    write_marker(fs, SEQUENTIAL_MARKER_PREFIX, session_id);
}

/// Write the task-created marker for this session.
pub fn mark_task_created(fs: &dyn FileSystemPort, session_id: &str) {
    write_marker(fs, TASK_MARKER_PREFIX, session_id);
}

/// Write the plan-approved marker for this session (ExitPlanMode was called).
pub fn mark_plan_approved(fs: &dyn FileSystemPort, session_id: &str) {
    write_marker(fs, PLAN_MARKER_PREFIX, session_id);
}

/// Write the task-active marker for this session (a task is in_progress).
pub fn mark_task_active(fs: &dyn FileSystemPort, session_id: &str) {
    write_marker(fs, TASK_ACTIVE_PREFIX, session_id);
}

/// Best-effort lookup for the most recent pending task ID in this project,
/// to give the user an actionable ID in the block message. Returns
/// `Some("Task #N is pending — …")` when a pending task file is found,
/// `None` otherwise.
///
/// The persisted task store lives at
/// `~/.claude/persistent-tasks/{project_hash}/tasks.json` (see
/// `task_persist.rs`). We don't have the project hash here without
/// more plumbing, so we scan the `persistent-tasks/*/tasks.json` tree
/// and pick whichever JSON has a pending entry. This is a hint, not a
/// source of truth — a None return degrades gracefully to the generic
/// message.
fn recent_pending_task_hint(fs: &dyn FileSystemPort, _session_id: &str) -> Option<String> {
    let home = fs.home_dir()?;
    let root = home.join(".claude").join("persistent-tasks");
    if !fs.is_dir(&root) {
        return None;
    }
    let projects = fs.read_dir(&root).ok()?;
    for proj in projects {
        let tasks_file = proj.join("tasks.json");
        if !fs.exists(&tasks_file) {
            continue;
        }
        let content = match fs.read_to_string(&tasks_file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let tasks = match json.get("tasks").and_then(|t| t.as_array()) {
            Some(t) => t,
            None => continue,
        };
        for task in tasks {
            let status = task.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status != "pending" {
                continue;
            }
            let id = task
                .get("id")
                .and_then(|v| v.as_str())
                .or_else(|| task.get("id").and_then(|v| v.as_i64()).map(|_| "?"))
                .unwrap_or("?");
            let subject = task
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or("(no subject)");
            return Some(format!("Task #{id} is pending: \"{subject}\"."));
        }
    }
    None
}

/// Process a PreToolUse event. Blocks Edit/Write if preconditions aren't met.
pub fn process(input: &HookInput, fs: &dyn FileSystemPort, env: &dyn EnvPort) -> HookOutput {
    let tool = match &input.tool_name {
        Some(t) => t.as_str(),
        None => return HookOutput::allow(),
    };

    // Only gate Edit and Write — not Bash or MCP tools
    if tool != "Edit" && tool != "Write" {
        return HookOutput::allow();
    }

    let session_id = match &input.session_id {
        Some(id) if !id.is_empty() => id.as_str(),
        _ => return HookOutput::allow(),
    };

    // Universal bypass: a signed `verification` override (activated this
    // session via the user saying "override verification") unblocks ALL four
    // preconditions below for the override TTL. Previously this override only
    // affected pre_commit_verification — that was confusing ("why didn't my
    // override work?"). Now it's a consistent 60-second escape hatch for the
    // whole gate stack. The verification_override_path + is_signed_override_active
    // check still requires the hygiene_override hook to have written a
    // cryptographically-signed token (Attack #47 defence), so `touch`-based
    // bypass is still blocked.
    if is_signed_override_active(
        fs,
        &super::hygiene_override::verification_override_path(fs, session_id),
        "verification",
        session_id,
    ) {
        eprintln!("[sentinel] tool_usage_gate: allowed via active 'override verification'");
        return HookOutput::allow();
    }

    // Check 1: Sequential thinking must have been used this session
    if !has_marker(fs, SEQUENTIAL_MARKER_PREFIX, session_id) {
        return HookOutput::deny(
            "🔴 [Tool Usage Gate] BLOCKED: Use `mcp__sequential-thinking__sequentialthinking` \
             to think through your approach before making code changes."
        );
    }

    // Check 2: At least one task must exist this session
    if !has_marker(fs, TASK_MARKER_PREFIX, session_id) {
        return HookOutput::deny(
            "🔴 [Tool Usage Gate] BLOCKED: Create a task with `TaskCreate` (agent-team \
             harness) or `TodoWrite` (core Claude Code) before making code changes. \
             All work must be tracked as a task."
        );
    }

    // Check 3: The session must be in plan mode (real 2.1.114 signal), OR the
    // plan-approved marker is set (ExitPlanMode fired during this session), OR
    // a recent plan file exists in `{cwd}/plans/` (resumed-session fallback).
    //
    // Plan mode entry paths (2.1.114): (a) Shift+Tab in the UI, (b) the
    // `EnterPlanMode` tool (real in the compiled binary — handler `r7H` —
    // though omitted from `sdk-tools.d.ts`; rejects inside agent contexts),
    // (c) env var `CLAUDE_CODE_PLAN_MODE_REQUIRED=1`, (d) `Agent` tool with
    // `mode: "plan"`, (e) agent YAML frontmatter `permissionMode: "plan"`, or
    // (f) CLI flag `--permission-mode plan`. `ExitPlanMode` is the approval
    // step that commits the plan.
    //
    // Primary signal: `detect_plan_mode_from_transcript` reads the live
    // transcript and returns true when the last plan-related tool_use is
    // `EnterPlanMode` (not yet followed by `ExitPlanMode`). This is the
    // authoritative source.
    //
    // SENTINEL_AUTOPILOT is intentionally *only* consulted when no
    // transcript path was provided — it's a last-resort escape hatch for
    // malformed harness input, not a user-facing bypass.
    let in_plan_mode = input
        .transcript_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .map(|p| detect_plan_mode_from_transcript(Path::new(p)));

    let plan_check_ok = match in_plan_mode {
        Some(true) => true,
        Some(false) => {
            has_marker(fs, PLAN_MARKER_PREFIX, session_id)
                || has_recent_plan_file(fs, input.cwd.as_deref(), SystemTime::now())
        }
        None => {
            // No transcript available — fall back to markers, the plan-file
            // heuristic, and finally the SENTINEL_AUTOPILOT escape hatch.
            is_autopilot(env)
                || has_marker(fs, PLAN_MARKER_PREFIX, session_id)
                || has_recent_plan_file(fs, input.cwd.as_deref(), SystemTime::now())
        }
    };

    if !plan_check_ok {
        return HookOutput::deny(
            "🔴 [Tool Usage Gate] BLOCKED: Plan Mode is required. Enter plan mode via \
             `EnterPlanMode` (or Shift+Tab, `CLAUDE_CODE_PLAN_MODE_REQUIRED=1`, \
             `Agent(mode:\"plan\")`, or `--permission-mode plan`). Then call \
             `ExitPlanMode` with the plan content for approval. Alternatively, \
             place a recent `.md` plan file under `plans/` in your CURRENT shell \
             cwd (resumed-session fallback) — if you're inside a git worktree, \
             the walk-up stops at the worktree's `.git` file, so the plan MUST \
             live inside the worktree itself (e.g. `{worktree}/plans/foo.md`), \
             not at the main repo root."
        );
    }

    // Check 4: A task must be actively in_progress.
    //
    // Normally satisfied by `mark_task_active` firing on a PostToolUse for
    // TaskUpdate(status="in_progress") or a TodoWrite whose payload already
    // has an item in_progress. As of sentinel main (late April 2026), we
    // *also* activate on `TaskCreate` / `TodoWrite` creation — agents usually
    // create a task and start working on it in the same turn, and forcing a
    // dedicated TaskUpdate turn before any Edit is pure friction.
    if !has_marker(fs, TASK_ACTIVE_PREFIX, session_id) {
        let hint = recent_pending_task_hint(fs, session_id).unwrap_or_default();
        let msg = if hint.is_empty() {
            "🔴 [Tool Usage Gate] BLOCKED: Create a task with `TaskCreate` (agent-team \
             harness) or `TodoWrite` (core Claude Code) before making code changes. \
             All work must be tracked as an active task.".to_string()
        } else {
            format!(
                "🔴 [Tool Usage Gate] BLOCKED: Mark a task as `in_progress` before making \
                 code changes. {hint} Use `TaskUpdate(taskId: \"<id>\", \
                 status: \"in_progress\")` or update a `TodoWrite` entry's status \
                 to `in_progress`."
            )
        };
        return HookOutput::deny(msg);
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct MockFs {
        existing_files: Mutex<HashSet<PathBuf>>,
    }

    impl MockFs {
        fn new() -> Self {
            Self { existing_files: Mutex::new(HashSet::new()) }
        }

        fn with_marker(prefix: &str, session_id: &str) -> Self {
            let fs = Self::new();
            let path = temp_marker_path(prefix, session_id);
            fs.existing_files.lock().unwrap().insert(path);
            fs
        }

        fn with_all_markers(session_id: &str) -> Self {
            let fs = Self::new();
            for prefix in [
                SEQUENTIAL_MARKER_PREFIX,
                TASK_MARKER_PREFIX,
                PLAN_MARKER_PREFIX,
                TASK_ACTIVE_PREFIX,
            ] {
                fs.existing_files
                    .lock()
                    .unwrap()
                    .insert(temp_marker_path(prefix, session_id));
            }
            fs
        }

        fn with_markers(session_id: &str, prefixes: &[&str]) -> Self {
            let fs = Self::new();
            for prefix in prefixes {
                fs.existing_files
                    .lock()
                    .unwrap()
                    .insert(temp_marker_path(prefix, session_id));
            }
            fs
        }
    }

    impl FileSystemPort for MockFs {
        fn home_dir(&self) -> Option<PathBuf> { Some(PathBuf::from("/mock/home")) }
        fn read_to_string(&self, _: &Path) -> anyhow::Result<String> { anyhow::bail!("not found") }
        fn write(&self, path: &Path, _: &[u8]) -> anyhow::Result<()> {
            self.existing_files.lock().unwrap().insert(path.to_path_buf());
            Ok(())
        }
        fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> { Ok(()) }
        fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> { Ok(vec![]) }
        fn exists(&self, path: &Path) -> bool {
            self.existing_files.lock().unwrap().contains(path)
        }
        fn is_dir(&self, _: &Path) -> bool { false }
        fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> { anyhow::bail!("no") }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> { Ok(()) }
    }

    fn edit_input(session_id: &str) -> HookInput {
        HookInput {
            tool_name: Some("Edit".to_string()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        }
    }

    fn write_input(session_id: &str) -> HookInput {
        HookInput {
            tool_name: Some("Write".to_string()),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_allows_non_edit_write_tools() {
        let fs = MockFs::new();
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &fs, &crate::hooks::test_support::StubEnv::new()).blocked.is_none());
    }

    #[test]
    fn test_allows_mcp_tools() {
        let fs = MockFs::new();
        let input = HookInput {
            tool_name: Some("mcp__linear__create_issue".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &fs, &crate::hooks::test_support::StubEnv::new()).blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_session_id() {
        let fs = MockFs::new();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &fs, &crate::hooks::test_support::StubEnv::new()).blocked.is_none());
    }

    #[test]
    fn test_blocks_edit_without_sequential_thinking() {
        let fs = MockFs::new();
        let output = process(&edit_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_write_without_sequential_thinking() {
        let fs = MockFs::new();
        let output = process(&write_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_edit_without_task_but_with_sequential() {
        let fs = MockFs::with_marker(SEQUENTIAL_MARKER_PREFIX, "test-session");
        let output = process(&edit_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_edit_without_plan_approval() {
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
        ]);
        let output = process(&edit_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(output.blocked, Some(true));
        let reason = output.hook_specific_output.as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref()).unwrap_or("");
        // Message references the real entry paths: EnterPlanMode (real tool
        // per 2.1.114 binary handler `r7H`, though hidden from sdk-tools.d.ts),
        // Shift+Tab, env var, Agent mode, or CLI flag; and ExitPlanMode as the
        // approval step.
        assert!(reason.contains("Plan Mode") && reason.contains("ExitPlanMode"));
        assert!(reason.contains("EnterPlanMode"),
            "deny message must reference EnterPlanMode — real tool per 2.1.114 audit");
        assert!(reason.contains("worktree"),
            "deny message must warn that walk-up stops at worktree .git boundary");
        assert!(reason.contains("current shell cwd") || reason.contains("CURRENT shell cwd"),
            "deny message must clarify cwd means the current shell directory, not repo root");
    }

    #[test]
    fn test_blocks_edit_without_active_task() {
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            PLAN_MARKER_PREFIX,
        ]);
        let output = process(&edit_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(output.blocked, Some(true));
        let reason = output.hook_specific_output.as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref()).unwrap_or("");
        assert!(
            reason.contains("in_progress") || reason.contains("TaskCreate"),
            "block message should mention in_progress or TaskCreate — got: {reason}",
        );
    }

    #[test]
    fn test_allows_edit_with_all_markers() {
        let fs = MockFs::with_all_markers("test-session");
        let output = process(&edit_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_write_with_all_markers() {
        let fs = MockFs::with_all_markers("test-session");
        let output = process(&write_input("test-session"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_write_marker_creates_file() {
        let fs = MockFs::new();
        assert!(!has_marker(&fs, SEQUENTIAL_MARKER_PREFIX, "s1"));
        mark_sequential_thinking_used(&fs, "s1");
        assert!(has_marker(&fs, SEQUENTIAL_MARKER_PREFIX, "s1"));
    }

    #[test]
    fn test_task_marker_creates_file() {
        let fs = MockFs::new();
        assert!(!has_marker(&fs, TASK_MARKER_PREFIX, "s1"));
        mark_task_created(&fs, "s1");
        assert!(has_marker(&fs, TASK_MARKER_PREFIX, "s1"));
    }

    #[test]
    fn test_plan_marker_creates_file() {
        let fs = MockFs::new();
        assert!(!has_marker(&fs, PLAN_MARKER_PREFIX, "s1"));
        mark_plan_approved(&fs, "s1");
        assert!(has_marker(&fs, PLAN_MARKER_PREFIX, "s1"));
    }

    #[test]
    fn test_task_active_marker_creates_file() {
        let fs = MockFs::new();
        assert!(!has_marker(&fs, TASK_ACTIVE_PREFIX, "s1"));
        mark_task_active(&fs, "s1");
        assert!(has_marker(&fs, TASK_ACTIVE_PREFIX, "s1"));
    }

    #[test]
    fn test_markers_are_session_scoped() {
        let fs = MockFs::with_all_markers("session-a");
        let output = process(&edit_input("session-b"), &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_active_verification_override_bypasses_all_checks() {
        // An active signed verification override (user said "override verification")
        // must bypass the whole precondition stack — seq-thinking, task, plan,
        // active-task — so users have one escape hatch that actually works across
        // every gate this hook enforces. Needs a filesystem-backed port because
        // the signed-override check does real reads + signature verification.
        use super::super::hygiene_override::write_signed_override_for_test;

        let tmp = TempDir::new().unwrap();
        let session = "override-sess";
        let override_dir = tmp.path()
            .join(".claude")
            .join("sentinel")
            .join("overrides");
        fs::create_dir_all(&override_dir).unwrap();

        struct HomeFs {
            home: PathBuf,
        }
        impl FileSystemPort for HomeFs {
            fn home_dir(&self) -> Option<PathBuf> { Some(self.home.clone()) }
            fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
                Ok(fs::read_to_string(p)?)
            }
            fn write(&self, p: &Path, b: &[u8]) -> anyhow::Result<()> {
                fs::write(p, b)?;
                Ok(())
            }
            fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
                fs::create_dir_all(p)?;
                Ok(())
            }
            fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
                Ok(fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
            }
            fn exists(&self, p: &Path) -> bool { p.exists() }
            fn is_dir(&self, p: &Path) -> bool { p.is_dir() }
            fn metadata(&self, p: &Path) -> anyhow::Result<fs::Metadata> { Ok(fs::metadata(p)?) }
            fn append(&self, p: &Path, b: &[u8]) -> anyhow::Result<()> {
                use std::io::Write;
                let mut f = fs::OpenOptions::new().append(true).create(true).open(p)?;
                f.write_all(b)?;
                Ok(())
            }
        }

        let fs_port = HomeFs { home: tmp.path().to_path_buf() };
        let override_path =
            super::super::hygiene_override::verification_override_path(&fs_port, session);
        write_signed_override_for_test(&fs_port, &override_path, "verification", session);

        // No markers set at all — normally every check would fire. With an
        // active override, all checks must be skipped and the edit is allowed.
        let input = HookInput {
            tool_name: Some("Edit".into()),
            session_id: Some(session.into()),
            ..Default::default()
        };
        let output = process(&input, &fs_port, &crate::hooks::test_support::StubEnv::new());
        assert!(
            output.blocked.is_none(),
            "active signed verification override must bypass the tool_usage_gate"
        );
    }

    fn autopilot_env() -> crate::hooks::test_support::StubEnv {
        crate::hooks::test_support::StubEnv::with(&[("SENTINEL_AUTOPILOT", "1")])
    }

    #[test]
    fn test_autopilot_fallback_when_no_transcript() {
        // SENTINEL_AUTOPILOT is only consulted when no transcript_path is
        // available in the hook input — it's a last-resort escape hatch,
        // not a user-facing bypass.
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            TASK_ACTIVE_PREFIX,
        ]);
        // `edit_input` omits transcript_path, so the None-branch fallback
        // kicks in and honours SENTINEL_AUTOPILOT.
        let output = process(&edit_input("test-session"), &fs, &autopilot_env());
        assert!(output.blocked.is_none(),
            "autopilot env var must still work when no transcript is available");
    }

    #[test]
    fn test_autopilot_ignored_when_transcript_present_without_plan_signal() {
        // Once a transcript is available, it is authoritative. Autopilot
        // env var does NOT bypass the check — the model must actually
        // enter plan mode.
        let t = write_transcript(&[assistant_tool_use("Read")]);
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            TASK_ACTIVE_PREFIX,
        ]);
        let input = HookInput {
            tool_name: Some("Edit".into()),
            session_id: Some("test-session".into()),
            transcript_path: Some(t.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process(&input, &fs, &autopilot_env());
        assert_eq!(
            output.blocked,
            Some(true),
            "SENTINEL_AUTOPILOT must NOT bypass when transcript provides the real signal"
        );
    }

    #[test]
    fn test_autopilot_does_not_bypass_task_active_check() {
        // Plan marker missing AND task-active marker missing.
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
        ]);
        let output = process(&edit_input("test-session"), &fs, &autopilot_env());
        // Plan check skipped, but task-active still blocks.
        assert_eq!(output.blocked, Some(true));
        let reason = output.hook_specific_output.as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref()).unwrap_or("");
        assert!(
            reason.contains("in_progress") || reason.contains("TaskCreate"),
            "autopilot must still enforce the active-task check — got: {reason}",
        );
    }

    // ── has_recent_plan_file fallback ───────────────────────────────
    //
    // A separate FS mock that supports is_dir + read_dir + metadata so
    // we can exercise the resumed-session fallback path. The existing
    // MockFs only tracks `existing_files` and returns empty/default for
    // directory operations.

    use std::fs;
    use tempfile::TempDir;

    struct RealishFs {
        // Real FS-backed shim so metadata() returns actual timestamps.
    }
    impl FileSystemPort for RealishFs {
        fn home_dir(&self) -> Option<PathBuf> { None }
        fn read_to_string(&self, p: &Path) -> anyhow::Result<String> {
            Ok(fs::read_to_string(p)?)
        }
        fn write(&self, p: &Path, b: &[u8]) -> anyhow::Result<()> {
            fs::write(p, b)?;
            Ok(())
        }
        fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> {
            fs::create_dir_all(p)?;
            Ok(())
        }
        fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> {
            Ok(fs::read_dir(p)?.filter_map(|e| e.ok().map(|e| e.path())).collect())
        }
        fn exists(&self, p: &Path) -> bool { p.exists() }
        fn is_dir(&self, p: &Path) -> bool { p.is_dir() }
        fn metadata(&self, p: &Path) -> anyhow::Result<fs::Metadata> { Ok(fs::metadata(p)?) }
        fn append(&self, p: &Path, b: &[u8]) -> anyhow::Result<()> {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).create(true).open(p)?;
            f.write_all(b)?;
            Ok(())
        }
    }

    #[test]
    fn test_recent_plan_file_satisfies_plan_check() {
        let tmp = TempDir::new().unwrap();
        let plans = tmp.path().join("plans");
        fs::create_dir_all(&plans).unwrap();
        fs::write(plans.join("my-plan.md"), b"# Plan").unwrap();

        let fs_port = RealishFs {};
        assert!(has_recent_plan_file(&fs_port, tmp.path().to_str(), SystemTime::now()));
    }

    #[test]
    fn test_no_plan_file_means_no_fallback() {
        let tmp = TempDir::new().unwrap();
        // Seed a `.git` marker so the walk-up stops at the tempdir boundary
        // and doesn't bleed into real ancestor directories (e.g. ~/plans/)
        // that would accidentally satisfy the check on a dev machine.
        fs::write(tmp.path().join(".git"), b"gitdir: /fake").unwrap();
        fs::create_dir_all(tmp.path().join("plans")).unwrap();

        let fs_port = RealishFs {};
        assert!(!has_recent_plan_file(&fs_port, tmp.path().to_str(), SystemTime::now()));
    }

    #[test]
    fn test_missing_plans_dir_means_no_fallback() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".git"), b"gitdir: /fake").unwrap();
        let fs_port = RealishFs {};
        assert!(!has_recent_plan_file(&fs_port, tmp.path().to_str(), SystemTime::now()));
    }

    #[test]
    fn test_stale_plan_file_does_not_satisfy() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".git"), b"gitdir: /fake").unwrap();
        let plans = tmp.path().join("plans");
        fs::create_dir_all(&plans).unwrap();
        fs::write(plans.join("old.md"), b"# Old").unwrap();

        let fs_port = RealishFs {};
        // 8 days ago — past the 7-day window
        let future_now = SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60);
        assert!(!has_recent_plan_file(&fs_port, tmp.path().to_str(), future_now));
    }

    #[test]
    fn test_walk_up_finds_plan_in_parent_dir() {
        // cwd is a sub-dir; plans/ lives at the repo root above it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join(".git"), b"gitdir: /elsewhere").unwrap(); // worktree marker (file, not dir)
        let plans = root.join("plans");
        fs::create_dir_all(&plans).unwrap();
        fs::write(plans.join("root-plan.md"), b"# Plan").unwrap();

        let subdir = root.join("server").join("routes");
        fs::create_dir_all(&subdir).unwrap();

        let fs_port = RealishFs {};
        assert!(
            has_recent_plan_file(&fs_port, subdir.to_str(), SystemTime::now()),
            "walk-up should find plans/ at repo root"
        );
    }

    #[test]
    fn test_walk_up_stops_at_git_boundary() {
        // plans/ is ABOVE the repo root — walk-up should NOT reach it.
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path();
        let repo = outer.join("myrepo");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap(); // repo boundary

        // Plans live OUTSIDE the repo, above the .git boundary.
        let outer_plans = outer.join("plans");
        fs::create_dir_all(&outer_plans).unwrap();
        fs::write(outer_plans.join("outer-plan.md"), b"# Outer").unwrap();

        let fs_port = RealishFs {};
        assert!(
            !has_recent_plan_file(&fs_port, repo.to_str(), SystemTime::now()),
            "walk-up must stop at .git boundary and not find plans above repo root"
        );
    }

    #[test]
    fn test_walk_up_worktree_case() {
        // Simulates the exact shape of a git worktree:
        //   repo/.git/                  (real repo)
        //   repo/plans/my-plan.md       (plan lives at repo root)
        //   repo/.claude/worktrees/wt/  (worktree path = cwd)
        //   repo/.claude/worktrees/wt/.git  (worktree gitdir file)
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("plans")).unwrap();
        fs::write(repo.join("plans").join("fpcrm-358.md"), b"# Plan").unwrap();

        let wt = repo.join(".claude").join("worktrees").join("feature");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), b"gitdir: ../../../.git/worktrees/feature").unwrap();

        let fs_port = RealishFs {};
        // Worktree cwd has its own `.git` file (boundary) — if it also has
        // its own plans/, it'd match. Here it doesn't, so walk must climb
        // past the worktree's .git boundary. We intentionally DON'T — the
        // worktree is its own boundary. Callers should put plans/ inside
        // the worktree. This test asserts that behaviour.
        assert!(
            !has_recent_plan_file(&fs_port, wt.to_str(), SystemTime::now()),
            "worktree cwd with .git file is its own boundary; plans must live inside the worktree"
        );

        // But if we seed plans/ inside the worktree, it should find it.
        fs::create_dir_all(wt.join("plans")).unwrap();
        fs::write(wt.join("plans").join("wt-plan.md"), b"# WT").unwrap();
        assert!(
            has_recent_plan_file(&fs_port, wt.to_str(), SystemTime::now()),
            "plans/ inside the worktree itself should be found"
        );
    }

    // ── detect_plan_mode_from_transcript ────────────────────────────

    fn write_transcript(entries: &[serde_json::Value]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for e in entries {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
        f
    }

    fn assistant_tool_use(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "tool_use", "name": name, "input": {}}]
            }
        })
    }

    #[test]
    fn test_detect_plan_mode_returns_true_after_enter_plan_mode() {
        let t = write_transcript(&[assistant_tool_use("EnterPlanMode")]);
        assert!(detect_plan_mode_from_transcript(t.path()));
    }

    #[test]
    fn test_detect_plan_mode_returns_false_after_exit_plan_mode() {
        let t = write_transcript(&[
            assistant_tool_use("EnterPlanMode"),
            assistant_tool_use("ExitPlanMode"),
        ]);
        assert!(!detect_plan_mode_from_transcript(t.path()));
    }

    #[test]
    fn test_detect_plan_mode_returns_false_when_no_signal_present() {
        let t = write_transcript(&[assistant_tool_use("Read")]);
        assert!(!detect_plan_mode_from_transcript(t.path()));
    }

    #[test]
    fn test_detect_plan_mode_returns_false_when_file_missing() {
        assert!(!detect_plan_mode_from_transcript(Path::new("/does/not/exist")));
    }

    #[test]
    fn test_detect_plan_mode_uses_last_occurrence() {
        let t = write_transcript(&[
            assistant_tool_use("ExitPlanMode"),
            assistant_tool_use("Read"),
            assistant_tool_use("EnterPlanMode"),
        ]);
        assert!(detect_plan_mode_from_transcript(t.path()));
    }

    #[test]
    fn test_detect_plan_mode_ignores_user_messages() {
        let user_entry = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "tool_result", "content": "Entered plan mode."}]
            }
        });
        let t = write_transcript(&[user_entry]);
        assert!(!detect_plan_mode_from_transcript(t.path()));
    }

    #[test]
    fn test_transcript_plan_mode_allows_edit() {
        // Transcript shows EnterPlanMode — check #3 is satisfied by the
        // real 2.1.114 signal without any markers or env vars.
        let t = write_transcript(&[assistant_tool_use("EnterPlanMode")]);

        let fs = MockFs::with_markers("sess-plan", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            TASK_ACTIVE_PREFIX,
        ]);
        let input = HookInput {
            tool_name: Some("Edit".into()),
            session_id: Some("sess-plan".into()),
            transcript_path: Some(t.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process(&input, &fs, &crate::hooks::test_support::StubEnv::new());
        assert!(
            output.blocked.is_none(),
            "transcript EnterPlanMode signal must satisfy plan check #3"
        );
    }

    #[test]
    fn test_transcript_without_plan_signal_blocks_edit() {
        let t = write_transcript(&[assistant_tool_use("Read")]);

        let fs = MockFs::with_markers("sess-noplan", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            TASK_ACTIVE_PREFIX,
        ]);
        let input = HookInput {
            tool_name: Some("Edit".into()),
            session_id: Some("sess-noplan".into()),
            transcript_path: Some(t.path().to_string_lossy().into_owned()),
            ..Default::default()
        };
        let output = process(&input, &fs, &crate::hooks::test_support::StubEnv::new());
        assert_eq!(
            output.blocked,
            Some(true),
            "without plan-mode signal, marker, or plan file, edit must be blocked"
        );
    }

    #[test]
    fn test_resumed_session_allowed_with_recent_plan() {
        let tmp = TempDir::new().unwrap();
        let plans = tmp.path().join("plans");
        fs::create_dir_all(&plans).unwrap();
        fs::write(plans.join("resumed.md"), b"# Plan").unwrap();

        // Sequential + task + task-active markers, but NO plan marker —
        // the resumed-session case. Should be allowed via the plan-file
        // fallback.
        let session = "resumed-sess";
        let marker_fs = MockFs::with_markers(session, &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            TASK_ACTIVE_PREFIX,
        ]);

        // Compose a FileSystemPort that delegates marker checks to
        // `marker_fs` (temp dir) and plan-dir checks to the real FS
        // scoped to `tmp`.
        struct Composite<'a> {
            markers: &'a MockFs,
            real: RealishFs,
        }
        impl FileSystemPort for Composite<'_> {
            fn home_dir(&self) -> Option<PathBuf> { None }
            fn read_to_string(&self, p: &Path) -> anyhow::Result<String> { self.real.read_to_string(p) }
            fn write(&self, p: &Path, b: &[u8]) -> anyhow::Result<()> { self.markers.write(p, b) }
            fn create_dir_all(&self, p: &Path) -> anyhow::Result<()> { self.real.create_dir_all(p) }
            fn read_dir(&self, p: &Path) -> anyhow::Result<Vec<PathBuf>> { self.real.read_dir(p) }
            fn exists(&self, p: &Path) -> bool {
                // Marker checks go to temp dir; plan-file checks go to real FS.
                if p.to_string_lossy().contains("claude-") {
                    self.markers.exists(p)
                } else {
                    self.real.exists(p)
                }
            }
            fn is_dir(&self, p: &Path) -> bool { self.real.is_dir(p) }
            fn metadata(&self, p: &Path) -> anyhow::Result<fs::Metadata> { self.real.metadata(p) }
            fn append(&self, p: &Path, b: &[u8]) -> anyhow::Result<()> { self.real.append(p, b) }
        }

        let fs_port = Composite { markers: &marker_fs, real: RealishFs {} };
        let input = HookInput {
            tool_name: Some("Edit".into()),
            session_id: Some(session.into()),
            cwd: Some(tmp.path().to_string_lossy().into()),
            ..Default::default()
        };
        let output = process(&input, &fs_port, &crate::hooks::test_support::StubEnv::new());
        assert!(output.blocked.is_none(), "plan-file fallback should allow write");
    }

    // ── Edge-case tests for detect_plan_mode_from_transcript ────────

    #[test]
    fn test_detect_plan_mode_handles_malformed_json_lines() {
        // Mix valid JSON lines with garbage. The last valid plan signal
        // (EnterPlanMode) must win despite invalid lines interspersed.
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Line 1: valid — ExitPlanMode
        writeln!(f, "{}", serde_json::to_string(&assistant_tool_use("ExitPlanMode")).unwrap()).unwrap();
        // Line 2: garbage
        writeln!(f, "not valid json {{{{").unwrap();
        // Line 3: valid — EnterPlanMode (most recent valid signal)
        writeln!(f, "{}", serde_json::to_string(&assistant_tool_use("EnterPlanMode")).unwrap()).unwrap();
        // Line 4: more garbage after the last valid signal
        writeln!(f, "{{broken").unwrap();
        // Walking backwards: line 4 is skipped (malformed), line 3 is
        // EnterPlanMode → returns true without reading further.
        assert!(
            detect_plan_mode_from_transcript(f.path()),
            "malformed lines must be skipped; last valid signal (EnterPlanMode) must win"
        );
    }

    #[test]
    fn test_detect_plan_mode_handles_empty_file() {
        // A zero-byte transcript must not panic and must return false.
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Write nothing — empty file.
        f.flush().unwrap();
        assert!(
            !detect_plan_mode_from_transcript(f.path()),
            "empty transcript must return false without panicking"
        );
    }

    #[test]
    fn test_detect_plan_mode_multiple_tool_uses_in_same_message() {
        // An assistant message whose content array contains BOTH a Read
        // tool_use AND an EnterPlanMode tool_use. The function should detect
        // the plan-mode signal even when it shares a message with other tools.
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "Read", "input": {}},
                    {"type": "tool_use", "name": "EnterPlanMode", "input": {}}
                ]
            }
        });
        let t = write_transcript(&[entry]);
        assert!(
            detect_plan_mode_from_transcript(t.path()),
            "EnterPlanMode in a multi-tool-use message must be detected"
        );
    }

    #[test]
    fn test_detect_plan_mode_in_message_ordering_last_wins() {
        // Single assistant message whose content array lists ExitPlanMode
        // before EnterPlanMode. Chronologically, EnterPlanMode is the later
        // action within this message, so the current state is plan mode.
        let entry = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "name": "ExitPlanMode", "input": {}},
                    {"type": "tool_use", "name": "EnterPlanMode", "input": {}}
                ]
            }
        });
        let t = write_transcript(&[entry]);
        assert!(
            detect_plan_mode_from_transcript(t.path()),
            "within a single message the latest tool_use must win — EnterPlanMode after ExitPlanMode should yield plan mode"
        );
    }

    #[test]
    fn test_detect_plan_mode_ignores_unrelated_tool_names() {
        // Transcript contains only Bash, Read, and Edit tool_uses — no plan
        // signal at all. Must return false.
        let t = write_transcript(&[
            assistant_tool_use("Bash"),
            assistant_tool_use("Read"),
            assistant_tool_use("Edit"),
        ]);
        assert!(
            !detect_plan_mode_from_transcript(t.path()),
            "unrelated tool names must not trigger plan-mode detection"
        );
    }

    #[test]
    fn test_detect_plan_mode_handles_very_long_transcript() {
        // Generate 1000+ lines. EnterPlanMode appears at line ~500,
        // ExitPlanMode appears as the very LAST line. Walking backwards the
        // function must find ExitPlanMode first and return false.
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();

        // Lines 1–499: unrelated Read tool_uses
        for _ in 0..499 {
            writeln!(
                f,
                "{}",
                serde_json::to_string(&assistant_tool_use("Read")).unwrap()
            )
            .unwrap();
        }
        // Line 500: EnterPlanMode
        writeln!(
            f,
            "{}",
            serde_json::to_string(&assistant_tool_use("EnterPlanMode")).unwrap()
        )
        .unwrap();
        // Lines 501–999: more unrelated tool_uses
        for _ in 0..499 {
            writeln!(
                f,
                "{}",
                serde_json::to_string(&assistant_tool_use("Bash")).unwrap()
            )
            .unwrap();
        }
        // Line 1000 (last): ExitPlanMode — this must win because we walk backwards
        writeln!(
            f,
            "{}",
            serde_json::to_string(&assistant_tool_use("ExitPlanMode")).unwrap()
        )
        .unwrap();

        assert!(
            !detect_plan_mode_from_transcript(f.path()),
            "last line is ExitPlanMode so result must be false (last signal wins)"
        );
    }
}
