//! Tool Usage Gate
//!
//! PreToolUse hook that blocks Edit/Write if required preconditions aren't met:
//! 1. Sequential thinking must have been used this session
//! 2. At least one task must have been created this session
//! 3. A plan must have been approved this session (ExitPlanMode called)
//! 4. A task must be actively in_progress
//!
//! State is tracked via marker files in the temp directory, keyed by session ID.
//! Marker files are written by the PostToolUse dispatcher when it detects
//! the relevant tool calls.
//!
//! Plan-approval fallback: when a session is resumed, `ExitPlanMode` may have
//! been called in a prior session, so no PLAN_MARKER exists. Detect this by
//! scanning `{cwd}/plans/*.md` for a file written within the last 7 days —
//! Claude Code writes plans there on approval, so presence of a recent plan
//! file is evidence of an approved plan for this working directory.

use sentinel_domain::events::{HookInput, HookOutput};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::FileSystemPort;

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

/// Check whether `{cwd}/plans/` contains a recently-written `*.md` file.
/// This is the fallback for the PLAN_MARKER when a session is resumed —
/// Claude Code persists approved plans as markdown here, so presence of a
/// recent one is evidence of prior plan approval for this working directory.
fn has_recent_plan_file(fs: &dyn FileSystemPort, cwd: Option<&str>, now: SystemTime) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let plans_dir = Path::new(cwd).join("plans");
    if !fs.is_dir(&plans_dir) {
        return false;
    }
    let entries = match fs.read_dir(&plans_dir) {
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

/// Process a PreToolUse event. Blocks Edit/Write if preconditions aren't met.
pub fn process(input: &HookInput, fs: &dyn FileSystemPort) -> HookOutput {
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

    // Check 1: Sequential thinking must have been used this session
    if !has_marker(fs, SEQUENTIAL_MARKER_PREFIX, session_id) {
        return HookOutput::deny(
            "[Tool Usage Gate] BLOCKED: Use `mcp__sequential-thinking__sequentialthinking` \
             to think through your approach before making code changes."
        );
    }

    // Check 2: At least one task must exist this session
    if !has_marker(fs, TASK_MARKER_PREFIX, session_id) {
        return HookOutput::deny(
            "[Tool Usage Gate] BLOCKED: Create a task with `TaskCreate` before making \
             code changes. All work must be tracked as a task."
        );
    }

    // Check 3: A plan must have been approved this session, OR a recent plan
    // file exists in {cwd}/plans/ (resumed-session fallback).
    if !has_marker(fs, PLAN_MARKER_PREFIX, session_id)
        && !has_recent_plan_file(fs, input.cwd.as_deref(), SystemTime::now())
    {
        return HookOutput::deny(
            "[Tool Usage Gate] BLOCKED: Use `EnterPlanMode` to design your approach, \
             then `ExitPlanMode` to get approval before making code changes. \
             Plan Mode is required for all implementation work."
        );
    }

    // Check 4: A task must be actively in_progress
    if !has_marker(fs, TASK_ACTIVE_PREFIX, session_id) {
        return HookOutput::deny(
            "[Tool Usage Gate] BLOCKED: Mark a task as `in_progress` with \
             `TaskUpdate(taskId, status: \"in_progress\")` before making code changes. \
             No work should happen without an active task."
        );
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
        assert!(process(&input, &fs).blocked.is_none());
    }

    #[test]
    fn test_allows_mcp_tools() {
        let fs = MockFs::new();
        let input = HookInput {
            tool_name: Some("mcp__linear__create_issue".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &fs).blocked.is_none());
    }

    #[test]
    fn test_allows_when_no_session_id() {
        let fs = MockFs::new();
        let input = HookInput {
            tool_name: Some("Edit".to_string()),
            ..Default::default()
        };
        assert!(process(&input, &fs).blocked.is_none());
    }

    #[test]
    fn test_blocks_edit_without_sequential_thinking() {
        let fs = MockFs::new();
        let output = process(&edit_input("test-session"), &fs);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_write_without_sequential_thinking() {
        let fs = MockFs::new();
        let output = process(&write_input("test-session"), &fs);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_edit_without_task_but_with_sequential() {
        let fs = MockFs::with_marker(SEQUENTIAL_MARKER_PREFIX, "test-session");
        let output = process(&edit_input("test-session"), &fs);
        assert_eq!(output.blocked, Some(true));
    }

    #[test]
    fn test_blocks_edit_without_plan_approval() {
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
        ]);
        let output = process(&edit_input("test-session"), &fs);
        assert_eq!(output.blocked, Some(true));
        let reason = output.hook_specific_output.as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref()).unwrap_or("");
        assert!(reason.contains("EnterPlanMode"));
    }

    #[test]
    fn test_blocks_edit_without_active_task() {
        let fs = MockFs::with_markers("test-session", &[
            SEQUENTIAL_MARKER_PREFIX,
            TASK_MARKER_PREFIX,
            PLAN_MARKER_PREFIX,
        ]);
        let output = process(&edit_input("test-session"), &fs);
        assert_eq!(output.blocked, Some(true));
        let reason = output.hook_specific_output.as_ref()
            .and_then(|h| h.permission_decision_reason.as_deref()).unwrap_or("");
        assert!(reason.contains("in_progress"));
    }

    #[test]
    fn test_allows_edit_with_all_markers() {
        let fs = MockFs::with_all_markers("test-session");
        let output = process(&edit_input("test-session"), &fs);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_write_with_all_markers() {
        let fs = MockFs::with_all_markers("test-session");
        let output = process(&write_input("test-session"), &fs);
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
        let output = process(&edit_input("session-b"), &fs);
        assert_eq!(output.blocked, Some(true));
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
        fs::create_dir_all(tmp.path().join("plans")).unwrap();

        let fs_port = RealishFs {};
        assert!(!has_recent_plan_file(&fs_port, tmp.path().to_str(), SystemTime::now()));
    }

    #[test]
    fn test_missing_plans_dir_means_no_fallback() {
        let tmp = TempDir::new().unwrap();
        let fs_port = RealishFs {};
        assert!(!has_recent_plan_file(&fs_port, tmp.path().to_str(), SystemTime::now()));
    }

    #[test]
    fn test_stale_plan_file_does_not_satisfy() {
        let tmp = TempDir::new().unwrap();
        let plans = tmp.path().join("plans");
        fs::create_dir_all(&plans).unwrap();
        fs::write(plans.join("old.md"), b"# Old").unwrap();

        let fs_port = RealishFs {};
        // 8 days ago — past the 7-day window
        let future_now = SystemTime::now() + Duration::from_secs(8 * 24 * 60 * 60);
        assert!(!has_recent_plan_file(&fs_port, tmp.path().to_str(), future_now));
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
        let output = process(&input, &fs_port);
        assert!(output.blocked.is_none(), "plan-file fallback should allow write");
    }
}
