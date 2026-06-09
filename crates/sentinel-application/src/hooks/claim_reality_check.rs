//! Claim Reality Check — Stop hook
//!
//! Institutionalizes false-done detection. When Claude finishes a turn, this
//! hook sweeps the session task list for tasks that are marked `completed`
//! (the ✅ ones) whose subject/description makes a concrete-artifact claim
//! (a commit sha, a PR #, "merged", …) and reality-checks that claim against
//! git/gh. If a *completed* task's claim does NOT hold — the named commit isn't
//! on HEAD, or a claimed-merged PR is actually open — it injects a clear
//! reality-check warning so the agent can reopen or fix the task.
//!
//! This is the **sweep** counterpart to [`super::task_completed`], which runs
//! the same checks **per-completion**. The claim detection
//! ([`super::task_completed::detect_claims`]) and the real git/gh verification
//! ([`super::task_completed::verify_claims`]) are reused verbatim — this hook
//! adds only the "find newly-completed tasks and don't re-flag" sweep logic.
//!
//! ## Hard-mismatch only
//! `verify_claims` emits both *hard* mismatches (claim provably false) and
//! *soft* confirm notes (e.g. "could not run gh"). On a per-Stop sweep over
//! already-completed tasks, the soft notes would fire every turn and become
//! noise, so this hook filters to **hard mismatches only** (see
//! [`is_hard_mismatch`]). Soft "couldn't verify" cases are silently skipped.
//!
//! ## Throttle / don't re-flag
//! A per-session marker at
//! `~/.claude/sentinel/state/reality-check-{session_id}` records the id of
//! every completed task that has already been checked (whether it passed or was
//! flagged). Each Stop only checks completed tasks NOT already in the marker, so
//! the same task is never re-evaluated turn after turn.
//!
//! ## Fail-open contract
//! Any error — no session, unreadable tasks, git/gh failure, panic — returns
//! [`HookOutput::allow`] silently. This hook is reminder-only and must NEVER
//! block Stop.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::task_completed::{detect_claims, verify_claims};
use super::{FileSystemPort, HookContext};

/// Minimal task shape — only the fields this hook needs. Matches Claude Code's
/// on-disk task JSON (`~/.claude/tasks/{session}/{id}.json`); extra fields are
/// ignored. Includes `description` (unlike `task_coverage_check`'s `Task`)
/// because completed-task claims often live in the description.
#[derive(Debug, Clone, serde::Deserialize)]
struct Task {
    #[serde(default)]
    id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
}

/// Find the active session task dir (`~/.claude/tasks/{session_id}/`).
///
/// Mirrors `task_persist` / `task_coverage_check` so all three hooks read the
/// exact same set of task files. Returns `None` when the dir is absent or holds
/// no `.json` task files.
fn find_active_task_dir(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let session_dir = home.join(".claude").join("tasks").join(session_id);
    if fs.is_dir(&session_dir) && has_task_files(fs, &session_dir) {
        Some(session_dir)
    } else {
        None
    }
}

/// Does the dir contain at least one non-dotfile `.json` task file?
fn has_task_files(fs: &dyn FileSystemPort, dir: &PathBuf) -> bool {
    fs.read_dir(dir).is_ok_and(|entries| {
        entries.iter().any(|p| is_task_json(p))
    })
}

/// Is this path a non-dotfile `.json` file?
fn is_task_json(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Path::new(&name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        && !name.starts_with('.')
}

/// Read all tasks from the active session dir. Malformed files are skipped.
fn read_tasks(fs: &dyn FileSystemPort, dir: &PathBuf) -> Vec<Task> {
    let mut tasks = Vec::new();
    if let Ok(entries) = fs.read_dir(dir) {
        for path in entries {
            if !is_task_json(&path) {
                continue;
            }
            if let Ok(content) = fs.read_to_string(&path) {
                if let Ok(task) = serde_json::from_str::<Task>(&content) {
                    tasks.push(task);
                }
            }
        }
    }
    tasks
}

/// Per-session state dir: `~/.claude/sentinel/state`.
fn state_dir(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    Some(home.join(".claude").join("sentinel").join("state"))
}

/// Path of the per-session "already-checked completed task ids" marker.
fn checked_marker(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    Some(state_dir(fs)?.join(format!("reality-check-{session_id}")))
}

/// Read the set of completed-task ids already checked this session. One id per
/// line; blank lines skipped. `None`/missing/unreadable → empty set (so the
/// first sweep checks everything, fail-open).
fn read_checked(fs: &dyn FileSystemPort, session_id: &str) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Some(path) = checked_marker(fs, session_id) else {
        return set;
    };
    let Ok(content) = fs.read_to_string(&path) else {
        return set;
    };
    for line in content.lines() {
        let id = line.trim();
        if !id.is_empty() {
            set.insert(id.to_string());
        }
    }
    set
}

/// Persist the updated set of already-checked completed-task ids. Best-effort;
/// any error is ignored (worst case: a task gets re-checked next Stop).
fn write_checked(fs: &dyn FileSystemPort, session_id: &str, ids: &BTreeSet<String>) {
    let Some(path) = checked_marker(fs, session_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let mut body = String::new();
    for id in ids {
        body.push_str(id);
        body.push('\n');
    }
    let _ = fs.write(&path, body.as_bytes());
}

/// Is this `verify_claims` warning a HARD mismatch (claim provably false), as
/// opposed to a soft "couldn't verify" / "confirm before ✅" note?
///
/// `verify_claims` is shared with the per-completion gate, where soft notes are
/// useful. On the Stop sweep over already-completed tasks we only want to
/// surface claims that are actually contradicted by git/gh — otherwise every
/// completed task with an unparseable PR or a missing `gh` binary would flag on
/// every turn. We key off the distinctive phrases the hard-mismatch branches of
/// `verify_claims` emit.
fn is_hard_mismatch(warning: &str) -> bool {
    // Commit sha resolved but not reachable on HEAD.
    warning.contains("NOT on HEAD")
        // Claimed-merged PR that gh reports as not merged.
        || warning.contains("is claimed MERGED")
        // "committed" but the working tree is still dirty.
        || warning.contains("UNCOMMITTED")
        // Explicit completion promise with zero corroborating ground truth.
        || warning.contains("emitted completion promise")
}

/// Render the per-task reality-check warning block.
fn format_warning(task: &Task, hard: &[String]) -> String {
    let label = if task.subject.is_empty() {
        format!("#{}", task.id)
    } else {
        format!("#{} ('{}')", task.id, task.subject)
    };
    let details = hard
        .iter()
        .map(|w| format!("  - {w}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "⚠️ Reality-check: task {label} is marked ✅ but its claim doesn't hold:\n{details}"
    )
}

pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    // FAIL OPEN, hard: wrap the whole body so a panic anywhere (serde, git, gh)
    // can never block Stop. Returns allow() on any None/error inside.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(input, ctx)))
        .unwrap_or_else(|_| HookOutput::allow())
}

fn run(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    // Session id is required for the throttle marker. No session → fail open.
    let session_id = match input.session_id.as_deref() {
        Some(id) if !id.is_empty() => id,
        _ => return HookOutput::allow(),
    };

    // Read the session task list (same source as task_persist / coverage).
    let tasks = match find_active_task_dir(ctx.fs, session_id) {
        Some(dir) => read_tasks(ctx.fs, &dir),
        None => return HookOutput::allow(),
    };

    // Only the ✅ (completed) tasks are reality-checked here.
    let completed: Vec<&Task> = tasks.iter().filter(|t| t.status == "completed").collect();
    if completed.is_empty() {
        return HookOutput::allow();
    }

    // Throttle: skip tasks already checked this session.
    let mut checked = read_checked(ctx.fs, session_id);

    let mut warnings: Vec<String> = Vec::new();
    let mut newly_checked = false;

    for task in completed {
        if task.id.is_empty() || checked.contains(&task.id) {
            continue;
        }

        // Scan subject + description for concrete-artifact claims, then run the
        // SAME real verification task_completed uses (git merge-base, gh pr view).
        let scan_text = format!("{}\n{}", task.subject, task.description);
        let claims = detect_claims(&scan_text);

        // Mark checked regardless of outcome — passed OR flagged, we don't want
        // to re-evaluate it next Stop. (No claim → nothing to verify, still mark
        // so we never re-scan it.)
        checked.insert(task.id.clone());
        newly_checked = true;

        if !claims.any() {
            continue;
        }

        // `first_in_progress = false`: the "suspiciously-fast first hop" signal
        // is meaningless for an already-completed task in a sweep.
        let raw = verify_claims(claims, &scan_text, ctx, cwd, false);

        // Keep only HARD mismatches — soft "couldn't verify" notes would be
        // per-turn noise on the completed set.
        let hard: Vec<String> = raw.into_iter().filter(|w| is_hard_mismatch(w)).collect();
        if !hard.is_empty() {
            warnings.push(format_warning(task, &hard));
        }
    }

    // Persist the updated checked-set if we examined anything new.
    if newly_checked {
        write_checked(ctx.fs, session_id, &checked);
    }

    if warnings.is_empty() {
        return HookOutput::allow();
    }

    let context = format!(
        "[Claim Reality Check] One or more COMPLETED (✅) tasks claim concrete \
         work that git/gh does NOT corroborate. Reopen the task(s) with \
         `TaskUpdate` (status → in_progress) and finish the real work, or correct \
         the claim. Reminder only — sentinel does not change task status for you.\n\n{}",
        warnings.join("\n\n")
    );
    HookOutput::inject_context(HookEvent::Stop, &context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{StubEnv, StubMemoryMcp};
    use sentinel_domain::ports::{GitStatusPort, ProcessOutput, ProcessPort};
    use std::path::Path;

    /// Real-FS adapter scoped to a temp home so markers + task files stay
    /// isolated per test.
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

    /// FS whose every read fails — exercises the fail-open path.
    struct UnreadableFs {
        home: PathBuf,
    }
    impl FileSystemPort for UnreadableFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
        fn read_to_string(&self, _: &Path) -> anyhow::Result<String> {
            anyhow::bail!("boom")
        }
        fn write(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            anyhow::bail!("boom")
        }
        fn create_dir_all(&self, _: &Path) -> anyhow::Result<()> {
            anyhow::bail!("boom")
        }
        fn read_dir(&self, _: &Path) -> anyhow::Result<Vec<PathBuf>> {
            anyhow::bail!("boom")
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            true // claim the tasks dir exists so read_dir gets exercised + fails
        }
        fn metadata(&self, _: &Path) -> anyhow::Result<std::fs::Metadata> {
            anyhow::bail!("boom")
        }
        fn append(&self, _: &Path, _: &[u8]) -> anyhow::Result<()> {
            anyhow::bail!("boom")
        }
    }

    /// Git stub: clean tree, chosen HEAD presence.
    struct FakeGit {
        head: Option<String>,
        dirty: bool,
    }
    impl Default for FakeGit {
        fn default() -> Self {
            FakeGit {
                head: Some("abc1234".to_string()),
                dirty: false,
            }
        }
    }
    impl GitStatusPort for FakeGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            Ok(self.dirty)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> {
            Ok("main".into())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
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
            self.head.clone()
        }
    }

    /// Programmable process port mirroring task_completed's FakeProcess.
    struct FakeProcess {
        /// `Some(true)`=exit0 (reachable), `Some(false)`=exit1 (not ancestor),
        /// `None`=unresolved rev (exit1 + "bad revision" stderr).
        merge_base_ancestor: Option<bool>,
        gh_success: bool,
        gh_stdout: String,
    }
    impl Default for FakeProcess {
        fn default() -> Self {
            FakeProcess {
                merge_base_ancestor: Some(true),
                gh_success: true,
                gh_stdout: String::new(),
            }
        }
    }
    impl ProcessPort for FakeProcess {
        fn run(&self, command: &str, args: &[&str], _cwd: Option<&str>) -> anyhow::Result<ProcessOutput> {
            match command {
                "git" if args.first() == Some(&"merge-base") => match self.merge_base_ancestor {
                    Some(true) => Ok(ProcessOutput {
                        success: true,
                        stdout: String::new(),
                        stderr: String::new(),
                    }),
                    Some(false) => Ok(ProcessOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: String::new(),
                    }),
                    None => Ok(ProcessOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "fatal: Not a valid commit name deadbe1f".to_string(),
                    }),
                },
                "gh" => Ok(ProcessOutput {
                    success: self.gh_success,
                    stdout: self.gh_stdout.clone(),
                    stderr: String::new(),
                }),
                _ => Ok(ProcessOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }
        fn spawn_detached(&self, _: &str, _: &[&str]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn ctx_with<'a>(
        fs: &'a dyn FileSystemPort,
        git: &'a dyn GitStatusPort,
        process: &'a dyn ProcessPort,
        mem: &'a StubMemoryMcp,
        env: &'a StubEnv,
    ) -> HookContext<'a> {
        HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp: mem,
            env,
        }
    }

    /// Seed a session task dir with (id, subject, description, status) tasks.
    fn seed_tasks(home: &Path, session_id: &str, tasks: &[(&str, &str, &str, &str)]) {
        let dir = home.join(".claude").join("tasks").join(session_id);
        std::fs::create_dir_all(&dir).unwrap();
        for (id, subject, description, status) in tasks {
            let json = serde_json::json!({
                "id": id,
                "subject": subject,
                "description": description,
                "status": status,
            });
            std::fs::write(dir.join(format!("{id}.json")), json.to_string()).unwrap();
        }
    }

    fn input_for(session_id: &str, cwd: &str) -> HookInput {
        HookInput {
            session_id: Some(session_id.to_string()),
            cwd: Some(cwd.to_string()),
            ..Default::default()
        }
    }

    fn injected(out: &HookOutput) -> Option<String> {
        out.hook_specific_output
            .as_ref()
            .and_then(|h| h.additional_context.clone())
    }

    #[test]
    fn completed_with_valid_commit_on_head_no_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-valid";
        seed_tasks(
            &home,
            sid,
            &[("1", "Fix auth, committed at deadbe1f", "", "completed")],
        );

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(true), // sha IS on HEAD
            ..FakeProcess::default()
        };
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);

        let out = process(&input_for(sid, home.to_str().unwrap()), &ctx);
        assert!(
            injected(&out).is_none(),
            "a commit that IS on HEAD must not be flagged"
        );
    }

    #[test]
    fn completed_claiming_sha_not_on_head_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-badsha";
        seed_tasks(
            &home,
            sid,
            &[("7", "Landed the fix at deadbe1f", "", "completed")],
        );

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(false), // resolves but NOT an ancestor
            ..FakeProcess::default()
        };
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);

        let msg = injected(&process(&input_for(sid, home.to_str().unwrap()), &ctx))
            .expect("a sha not on HEAD must flag");
        assert!(msg.contains("Reality-check"), "{msg}");
        assert!(msg.contains("#7"), "{msg}");
        assert!(msg.contains("NOT on HEAD"), "{msg}");
    }

    #[test]
    fn completed_claiming_merged_pr_shows_open_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-pr";
        seed_tasks(
            &home,
            sid,
            &[("3", "Merged PR #42 to main", "all done", "completed")],
        );

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit::default();
        let proc = FakeProcess {
            gh_success: true,
            gh_stdout: r#"{"number":42,"state":"OPEN","merged":false}"#.to_string(),
            ..FakeProcess::default()
        };
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);

        let msg = injected(&process(&input_for(sid, home.to_str().unwrap()), &ctx))
            .expect("a claimed-merged PR that gh shows OPEN must flag");
        assert!(msg.contains("#3"), "{msg}");
        assert!(msg.contains("PR #42 is claimed MERGED"), "{msg}");
    }

    #[test]
    fn already_checked_task_not_reflagged() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-throttle";
        seed_tasks(
            &home,
            sid,
            &[("9", "Landed the fix at deadbe1f", "", "completed")],
        );

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(false),
            ..FakeProcess::default()
        };
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);
        let input = input_for(sid, home.to_str().unwrap());

        // First sweep: flags.
        let first = injected(&process(&input, &ctx));
        assert!(first.is_some(), "first sweep must flag the bad claim");
        // Marker should now record task 9.
        let checked = read_checked(&fs, sid);
        assert!(checked.contains("9"), "task 9 must be recorded as checked");

        // Second sweep, same (still-bad) task: must NOT re-flag.
        let second = injected(&process(&input, &ctx));
        assert!(
            second.is_none(),
            "an already-checked task must not be re-flagged: {second:?}"
        );
    }

    #[test]
    fn fail_open_no_session() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput {
            session_id: None,
            cwd: Some("/tmp".to_string()),
            ..Default::default()
        };
        let out = process(&input, &ctx);
        assert!(injected(&out).is_none());
        assert!(out.blocked.is_none());
    }

    #[test]
    fn fail_open_unreadable_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = UnreadableFs {
            home: tmp.path().to_path_buf(),
        };
        let git = FakeGit::default();
        let proc = FakeProcess::default();
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);
        let out = process(&input_for("sess-x", "/tmp"), &ctx);
        assert!(injected(&out).is_none());
        assert!(out.blocked.is_none());
    }

    #[test]
    fn non_completed_tasks_ignored() {
        // An in_progress task with a bad claim must NOT be flagged — this hook
        // only reality-checks ✅ tasks (task_coverage_check covers in_progress).
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-inprog";
        seed_tasks(
            &home,
            sid,
            &[("1", "Landed at deadbe1f", "", "in_progress")],
        );

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit::default();
        let proc = FakeProcess {
            merge_base_ancestor: Some(false),
            ..FakeProcess::default()
        };
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);

        let out = process(&input_for(sid, home.to_str().unwrap()), &ctx);
        assert!(
            injected(&out).is_none(),
            "in_progress tasks are not reality-checked here"
        );
    }

    #[test]
    fn completed_no_claim_no_flag() {
        // A completed task that makes no concrete-artifact claim → nothing to
        // verify → no flag (but it IS recorded so we never re-scan it).
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let sid = "sess-noclaim";
        seed_tasks(
            &home,
            sid,
            &[("2", "Investigated the slow query path", "", "completed")],
        );

        let fs = ScopedHomeFs { home: home.clone() };
        let git = FakeGit::default();
        let proc = FakeProcess::default();
        let mem = StubMemoryMcp;
        let env = StubEnv::new();
        let ctx = ctx_with(&fs, &git, &proc, &mem, &env);

        let out = process(&input_for(sid, home.to_str().unwrap()), &ctx);
        assert!(injected(&out).is_none(), "no claim → no flag");
        assert!(read_checked(&fs, sid).contains("2"), "still marked checked");
    }

    #[test]
    fn is_hard_mismatch_discriminates() {
        assert!(is_hard_mismatch("claimed commit deadbe1f is NOT on HEAD's history"));
        assert!(is_hard_mismatch("PR #42 is claimed MERGED but gh shows OPEN"));
        assert!(is_hard_mismatch("still has UNCOMMITTED changes"));
        assert!(is_hard_mismatch(
            "emitted completion promise `ALL_TESTS_PASSING` but NO corroborating ground truth"
        ));
        // Soft notes must NOT count as hard mismatches.
        assert!(!is_hard_mismatch("could not verify PR #42 (gh: not found)"));
        assert!(!is_hard_mismatch("claims a passing build/test — sentinel can't re-run it"));
    }
}
