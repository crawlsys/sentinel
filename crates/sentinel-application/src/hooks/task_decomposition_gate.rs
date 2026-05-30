//! Task Decomposition Gate — require a live, decomposed task list before
//! mutating work.
//!
//! The user's standing complaint is that "tasks are not always enforced":
//! the agent starts editing files / running state-changing commands without
//! ever decomposing the work into tracked tasks. This `PreToolUse` gate closes
//! that gap. Before a *mutating* tool is allowed, there must be evidence that
//! a decomposed task list exists for this session.
//!
//! Design mirrors `bug_task_gate` and `skill_invocation_gate`:
//!   - READ tools and Task* tools are NEVER gated. Gating them would deadlock
//!     the session — the very tools the agent uses to satisfy the gate
//!     (`TaskCreate`, `TaskList`, `Read`, sequential-thinking) must always
//!     pass, otherwise there is no fix path.
//!   - MUTATING tools (`Edit`, `Write`, `NotebookEdit`, and obvious
//!     state-changing `Bash` commands) are blocked ONLY when no live task
//!     list can be observed for the current session.
//!   - The "is there a live task list" check is best-effort: it reads the
//!     session's on-disk task files (`~/.claude/tasks/{session_id}/*.json`,
//!     the same files `task_persist` snapshots from). If that state cannot be
//!     read (no session id, no home dir, unreadable dir) the gate **fails
//!     open** — allow + log — rather than bricking the session.
//!
//! The block message is `[Sentinel-Authority]`-prefixed (added automatically
//! at the output boundary by `HookOutput::into_pretool_output`) and instructs
//! the agent to create a properly decomposed task list using the exact
//! conventions sentinel expects (numbered tasks, blocking dependency graph,
//! priority tags + emoji, real Na/Nb child subtasks, status CRUD).

use sentinel_domain::events::{HookEnvelope, HookInput, HookOutput};
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

/// Read-only / progress-toward-decomposition tools that must NEVER be gated.
/// This is the critical safety list: the tools the agent uses to *satisfy* the
/// gate (Task*, Read, Skill, sequential-thinking) and all pure-read tools.
/// Gating any of these deadlocks the session and the fix path.
const ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "LSP",
    "NotebookRead",
    "WebSearch",
    "WebFetch",
    "ToolSearch",
    "Skill",
    "TaskList",
    "TaskGet",
    "TaskCreate",
    "TaskUpdate",
    "TaskOutput",
];

/// Tools that are unconditionally treated as mutating.
const MUTATING_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

/// Obvious state-changing substrings in a Bash command. Conservative on
/// purpose — read-only bash (ls, cat, grep, git status/log/diff, find) must
/// pass. We only flag commands that clearly produce lasting change.
const MUTATING_BASH_MARKERS: &[&str] = &[
    "git commit",
    "git push",
    "git merge",
    "git reset",
    "git rebase",
    "git cherry-pick",
    "git stash",
    "git rm",
    "git mv",
    "git tag",
    "rm ",
    "rm -",
    "mv ",
    "cp ",
    "mkdir ",
    "touch ",
    "cargo build",
    "cargo install",
    "cargo publish",
    "npm install",
    "npm i ",
    "pnpm install",
    "yarn add",
    "yarn install",
    "pip install",
    "sed -i",
    "tee ",
    " > ",
    " >> ",
    ">>",
];

/// Whether the tool is on the always-allow list.
fn is_allowed_tool(tool_name: &str) -> bool {
    if ALLOWED_TOOLS.contains(&tool_name) {
        return true;
    }
    // Treat the entire sequential-thinking MCP namespace as a read tool.
    tool_name.starts_with("mcp__sequential-thinking__")
}

/// Extract the Bash command string from a `HookInput`'s `tool_input`.
fn bash_command(input: &HookInput) -> Option<String> {
    input
        .tool_input
        .as_ref()?
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Decide whether a Bash command is state-changing. Conservative: returns
/// `false` for read-only commands and `true` only when an obvious mutating
/// marker is present.
fn is_mutating_bash(command: &str) -> bool {
    // A redirection of any kind (`>`/`>>`) is a write; check raw first since
    // the trimmed marker list also covers spaced forms.
    if command.contains('>') {
        return true;
    }
    MUTATING_BASH_MARKERS
        .iter()
        .any(|marker| command.contains(marker))
}

/// Classify whether the tool about to run is a mutating operation that the
/// gate cares about. `Bash` is inspected; everything else is decided by name.
fn is_mutating_tool(input: &HookInput, tool_name: &str) -> bool {
    if MUTATING_TOOLS.contains(&tool_name) {
        return true;
    }
    if tool_name == "Bash" {
        return bash_command(input).is_some_and(|c| is_mutating_bash(&c));
    }
    false
}

/// Best-effort: does a live task list exist for this session?
///
/// Reads `~/.claude/tasks/{session_id}/*.json` (the same files `task_persist`
/// snapshots from). Returns:
///   - `Some(true)`  — at least one task file exists (a decomposed list is live).
///   - `Some(false)` — the session dir is readable but has no task files.
///   - `None`        — state could not be read (no session id, no home dir,
///                     unreadable dir). Callers FAIL OPEN on `None`.
fn has_live_task_list(fs: &dyn FileSystemPort, session_id: Option<&str>) -> Option<bool> {
    let session_id = session_id.filter(|s| !s.is_empty())?;
    let home = fs.home_dir()?;
    let session_dir = home.join(".claude").join("tasks").join(session_id);
    if !fs.is_dir(&session_dir) {
        // Dir doesn't exist yet — no tasks created this session. This is a
        // *readable* answer (the absence is definitive), so return Some(false)
        // rather than None: a brand-new session with no tasks SHOULD be gated.
        return Some(false);
    }
    let entries = fs.read_dir(&session_dir).ok()?;
    let has_task_file = entries.iter().any(|p| is_task_json(p));
    Some(has_task_file)
}

/// True for a non-dotfile `*.json` task file (skips `.lock`, `.highwatermark`).
fn is_task_json(path: &PathBuf) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.starts_with('.') {
        return false;
    }
    std::path::Path::new(&name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
}

/// The decomposition instructions emitted in the block message. Kept as a
/// single const so the message stays in lockstep with the documented
/// convention and tests can assert on its key markers.
const DECOMPOSITION_GUIDANCE: &str = "Before mutating work, create a decomposed task list with `TaskCreate`:\n\
  • DECOMPOSITION: break the work into numbered tasks (#1, #2, …) wired as BLOCKING tasks via addBlockedBy/addBlocks (a real dependency graph — these ARE native Claude Code Task fields).\n\
  • PRIORITY on every task (Claude Code has no native priority field, so use the convention): a [P0]/[P1]/[P2]/[P3] tag in the subject AND metadata.priority, list ordered by priority.\n\
  • TWO-EMOJI PREFIX on every subject: a STATUS emoji FIRST, then the PRIORITY color emoji, then the number. Status leads, priority follows — both always visible. e.g. `⬜ 🔴 1a [P0] — …`.\n\
  • PRIORITY color emoji (2nd position): 🔴 P0 critical, 🟠 P1 high, 🟡 P2 medium, 🟢 P3 low. NEVER dropped — it stays through every status.\n\
  • SUBTASKS as REAL child tasks (no native nesting, so use the convention): each subtask of #N is its OWN task, subject-prefixed `Na`/`Nb`/`Nc` (e.g. #1 → `1a — …`, `1b — …`, `1c — …`), with its own [P]/emoji/status, wired to the parent via addBlocks (subtask blocks parent) and chained to each other via addBlockedBy where ordered. Every subtask is independently tracked — own in_progress/completed — not a checklist line in prose.\n\
  • PARENT MIRROR CHECKLIST: each parent task's description holds a ☐/☑ checklist mirroring its child subtasks (`☑ Na — …` done, `☐ Nb — …` pending) so the parent shows subtask progress at a glance. The child tasks are the source of truth; the checklist tracks them.\n\
  • A descriptive explanation + a topical emoji per task.\n\
  • STATUS EMOJI (1st position): ⬜ pending, 🔄 in_progress, ✅ completed, ❌ failed. Update it on every transition (⬜→🔄 on start, 🔄→✅ on finish, 🔄→❌ on error) — the priority color in 2nd position never changes. On completion tick ☑ in the parent checklist; on failure leave ☐ with a note.\n\
  • Native status enum is pending|in_progress|completed|failed (failed IS valid — use ❌ and keep the task, don't delete it).\n\
  • Keep updated ALWAYS — proper CRUD the entire time: in_progress on start, completed + ✅ on finish (or failed if it errors), and re-decompose/add tasks as new work is discovered.";

/// `PreToolUse` handler — block mutating tools when no live decomposed task
/// list can be observed for the session. Read tools / Task* tools always pass.
/// Fails open (allow) when task state can't be read.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool_name = input.tool_name.as_deref().unwrap_or("");

    // Never gate read / task tools — they are the fix path.
    if is_allowed_tool(tool_name) {
        return HookOutput::allow();
    }

    // Only gate mutating operations.
    if !is_mutating_tool(input, tool_name) {
        return HookOutput::allow();
    }

    // Best-effort task-state check. FAIL OPEN on None (unreadable state).
    match has_live_task_list(ctx.fs, input.session_id.as_deref()) {
        Some(true) => HookOutput::allow(),
        None => {
            tracing::debug!(
                tool = tool_name,
                "task_decomposition_gate: task state unreadable — failing open (allow)"
            );
            HookOutput::allow()
        }
        Some(false) => {
            let envelope = HookEnvelope::block(
                "Task Decomposition Gate",
                format!(
                    "No live decomposed task list found for this session, but you are \
                     about to use a mutating tool (`{tool_name}`). {DECOMPOSITION_GUIDANCE}"
                ),
            );
            HookOutput::block(envelope.render())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx, StubFs};
    use std::path::{Path, PathBuf};

    /// FS that reports a caller-supplied home dir so tests can isolate
    /// `~/.claude/tasks/`.
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

    /// Build a `HookContext` whose FS is the supplied scoped-home adapter.
    fn ctx_with_fs(fs: &'static ScopedHomeFs) -> HookContext<'static> {
        let base = stub_ctx();
        HookContext {
            git: base.git,
            vector_store: None,
            fs,
            process: base.process,
            llm: None,
            memory_mcp: base.memory_mcp,
            env: base.env,
        }
    }

    /// Seed `~/.claude/tasks/{session}/1.json` with a task of the given status.
    fn seed_task(home: &Path, session: &str, status: &str) {
        let dir = home.join(".claude").join("tasks").join(session);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("1.json"),
            format!(
                r#"{{"id":"1","subject":"do thing","status":"{status}","blocks":[],"blockedBy":[]}}"#
            ),
        )
        .unwrap();
    }

    fn input(tool: &str, session: Option<&str>) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            session_id: session.map(str::to_string),
            ..Default::default()
        }
    }

    fn bash_input(command: &str, session: Option<&str>) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            session_id: session.map(str::to_string),
            tool_input: Some(serde_json::json!({ "command": command })),
            ..Default::default()
        }
    }

    // --- classification unit tests --------------------------------------

    #[test]
    fn read_tools_are_allowed() {
        for t in [
            "Read",
            "Glob",
            "Grep",
            "LS",
            "TaskList",
            "TaskGet",
            "NotebookRead",
            "WebFetch",
            "WebSearch",
        ] {
            assert!(is_allowed_tool(t), "{t} must be allowed");
        }
    }

    #[test]
    fn task_create_and_update_are_allowed() {
        assert!(is_allowed_tool("TaskCreate"));
        assert!(is_allowed_tool("TaskUpdate"));
    }

    #[test]
    fn sequential_thinking_namespace_is_allowed() {
        assert!(is_allowed_tool(
            "mcp__sequential-thinking__sequentialthinking"
        ));
    }

    #[test]
    fn edit_write_are_mutating() {
        assert!(is_mutating_tool(&input("Edit", None), "Edit"));
        assert!(is_mutating_tool(&input("Write", None), "Write"));
        assert!(is_mutating_tool(&input("NotebookEdit", None), "NotebookEdit"));
    }

    #[test]
    fn read_only_bash_is_not_mutating() {
        for c in [
            "ls -la",
            "cat README.md",
            "grep foo bar.rs",
            "git status",
            "git log --oneline",
            "git diff HEAD",
            "find . -name '*.rs'",
        ] {
            assert!(
                !is_mutating_bash(c),
                "read-only bash should not be mutating: {c}"
            );
        }
    }

    #[test]
    fn mutating_bash_is_detected() {
        for c in [
            "git commit -m x",
            "git push origin main",
            "git merge feat --no-edit",
            "git reset --hard",
            "rm -rf build",
            "mv a b",
            "cp src dst",
            "cargo build --release",
            "npm install",
            "sed -i 's/a/b/' f",
            "echo hi > out.txt",
            "cat a >> b",
        ] {
            assert!(is_mutating_bash(c), "should be mutating: {c}");
        }
    }

    // --- process() integration tests ------------------------------------

    #[test]
    fn read_tool_allows_even_without_tasks() {
        let ctx = stub_ctx();
        let out = process(&input("Read", Some("sess-1")), &ctx);
        assert!(out.blocked.is_none(), "Read must never be blocked");
    }

    #[test]
    fn task_create_allows_even_without_tasks() {
        let ctx = stub_ctx();
        let out = process(&input("TaskCreate", Some("sess-1")), &ctx);
        assert!(out.blocked.is_none(), "TaskCreate must never be blocked");
    }

    #[test]
    fn edit_with_no_task_list_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        }));
        let ctx = ctx_with_fs(fs);
        // No task dir seeded → Some(false) → block.
        let out = process(&input("Edit", Some("sess-empty")), &ctx);
        assert_eq!(out.blocked, Some(true), "Edit with no tasks must block");
        assert!(out
            .reason
            .as_ref()
            .is_some_and(|r| r.contains("decomposed task list")));
    }

    #[test]
    fn edit_with_live_task_list_allows() {
        let tmp = tempfile::tempdir().unwrap();
        seed_task(tmp.path(), "sess-live", "in_progress");
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        }));
        let ctx = ctx_with_fs(fs);
        let out = process(&input("Edit", Some("sess-live")), &ctx);
        assert!(
            out.blocked.is_none(),
            "Edit with a live task list must be allowed"
        );
    }

    #[test]
    fn read_only_bash_allows_without_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        }));
        let ctx = ctx_with_fs(fs);
        let out = process(&bash_input("git status", Some("sess-empty")), &ctx);
        assert!(
            out.blocked.is_none(),
            "read-only bash must be allowed even with no tasks"
        );
    }

    #[test]
    fn mutating_bash_with_no_tasks_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        }));
        let ctx = ctx_with_fs(fs);
        let out = process(&bash_input("git commit -m wip", Some("sess-empty")), &ctx);
        assert_eq!(
            out.blocked,
            Some(true),
            "mutating bash with no tasks must block"
        );
    }

    #[test]
    fn mutating_bash_with_live_tasks_allows() {
        let tmp = tempfile::tempdir().unwrap();
        seed_task(tmp.path(), "sess-live", "pending");
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        }));
        let ctx = ctx_with_fs(fs);
        let out = process(&bash_input("cargo build", Some("sess-live")), &ctx);
        assert!(
            out.blocked.is_none(),
            "mutating bash with a live task list must be allowed"
        );
    }

    #[test]
    fn fails_open_when_no_session_id() {
        // StubFs returns a home dir, but with no session id we can't locate
        // the task dir → None → fail open (allow).
        let ctx = stub_ctx();
        let out = process(&input("Edit", None), &ctx);
        assert!(
            out.blocked.is_none(),
            "missing session id must fail open (allow), never brick the session"
        );
    }

    #[test]
    fn has_live_task_list_none_without_session() {
        let fs = StubFs;
        assert!(has_live_task_list(&fs, None).is_none());
        assert!(has_live_task_list(&fs, Some("")).is_none());
    }

    #[test]
    fn has_live_task_list_false_for_empty_session_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp
            .path()
            .join(".claude")
            .join("tasks")
            .join("sess-x");
        std::fs::create_dir_all(&session_dir).unwrap();
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(has_live_task_list(&fs, Some("sess-x")), Some(false));
    }

    #[test]
    fn has_live_task_list_true_with_task_file() {
        let tmp = tempfile::tempdir().unwrap();
        seed_task(tmp.path(), "sess-y", "pending");
        let fs = ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        };
        assert_eq!(has_live_task_list(&fs, Some("sess-y")), Some(true));
    }

    #[test]
    fn block_message_contains_convention_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let fs: &'static ScopedHomeFs = Box::leak(Box::new(ScopedHomeFs {
            home: tmp.path().to_path_buf(),
        }));
        let ctx = ctx_with_fs(fs);
        let out = process(&input("Write", Some("sess-empty")), &ctx);
        let reason = out.reason.expect("block reason present");
        assert!(reason.contains("DECOMPOSITION"));
        assert!(reason.contains("PRIORITY"));
        assert!(reason.contains("SUBTASKS"));
        assert!(reason.contains("Na"));
        assert!(reason.contains("addBlockedBy"));
        assert!(reason.contains("MIRROR CHECKLIST"));
        assert!(reason.contains("STATUS EMOJI"));
        assert!(reason.contains("TWO-EMOJI"));
        assert!(reason.contains('⬜'));
        assert!(reason.contains('🔄'));
        assert!(reason.contains('✅'));
        assert!(reason.contains('❌'));
        assert!(reason.contains("pending|in_progress|completed|failed"));
    }
}
