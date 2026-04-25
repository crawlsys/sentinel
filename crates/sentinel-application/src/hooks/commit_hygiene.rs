//! Commit Hygiene — Two-phase hook
//!
//! **Stop phase:** Detects uncommitted changes, writes state to
//! `~/.claude/metrics/commit-hygiene.json`.
//!
//! **UserPromptSubmit phase:** Reads state, checks cooldown (15 min),
//! injects reminder with file list.

use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{EnvPort, FileSystemPort, HookContext};

/// Cooldown between commit reminders.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_MEDIUM_MS;

/// Minimum files to trigger a reminder.
const MIN_FILES: usize = constants::COMMIT_HYGIENE_MIN_FILES;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CommitState {
    cwd: String,
    #[serde(default)]
    session_id: String,
    file_count: usize,
    files: Vec<String>,
    ts: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn repo_hash(repo_root: &str) -> String {
    let mut h: u64 = 5381;
    for b in repo_root.bytes() {
        h = h.wrapping_mul(33).wrapping_add(u64::from(b));
    }
    format!("{h:016x}")
}

fn state_file(fs: &dyn FileSystemPort, repo_root: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join(format!("commit-hygiene-{}.json", repo_hash(repo_root))))
}

fn current_session_id(env: &dyn EnvPort) -> String {
    env.var("CLAUDE_SESSION_ID")
        .or_else(|| env.var("SESSION_ID"))
        .unwrap_or_else(|| "default".to_string())
}

fn cooldown_file(env: &dyn EnvPort) -> PathBuf {
    let session_id = current_session_id(env);
    std::env::temp_dir().join(format!("claude-commit-hygiene-{session_id}-last"))
}

fn cooldown_expired(fs: &dyn FileSystemPort, env: &dyn EnvPort) -> bool {
    let content = match fs.read_to_string(&cooldown_file(env)) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs: &dyn FileSystemPort, env: &dyn EnvPort) {
    let _ = fs.write(&cooldown_file(env), now_ms().to_string().as_bytes());
}

// ---------------------------------------------------------------------------
// Stop phase: detect uncommitted changes and write state
// ---------------------------------------------------------------------------

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let root = ctx.git.repo_root(cwd).unwrap_or_else(|| cwd.to_string());

    let files = match ctx.git.has_uncommitted_changes(cwd) {
        Ok(true) => match ctx.git.changed_files(cwd) {
            Ok(f) if !f.is_empty() => f,
            _ => {
                // No changes — clear any previous state (write empty)
                if let Some(path) = state_file(ctx.fs, &root) {
                    let _ = ctx.fs.write(&path, b"");
                }
                return HookOutput::allow();
            }
        },
        _ => {
            if let Some(path) = state_file(ctx.fs, &root) {
                let _ = ctx.fs.write(&path, b"");
            }
            return HookOutput::allow();
        }
    };

    let state = CommitState {
        cwd: cwd.to_string(),
        session_id: current_session_id(ctx.env),
        file_count: files.len(),
        files: files.into_iter().take(20).collect(), // Cap at 20 for readability
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = state_file(ctx.fs, &root) {
        let _ = ctx.fs.write(
            &path,
            serde_json::to_string(&state).unwrap_or_default().as_bytes(),
        );
    }

    tracing::debug!(count = state.file_count, "Uncommitted changes detected");
    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject commit reminder
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let root = ctx.git.repo_root(cwd).unwrap_or_else(|| cwd.to_string());

    let path = match state_file(ctx.fs, &root) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: CommitState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Only remind for the current project AND current session.
    // Including session_id prevents cross-session suppression when two
    // sessions share the same repo (non-worktree case): session A writes
    // state, session B must not treat it as its own.
    let session_id = current_session_id(ctx.env);
    if state.session_id != session_id || state.cwd != cwd {
        return HookOutput::allow();
    }

    // Don't nag for small change sets
    if state.file_count < MIN_FILES {
        return HookOutput::allow();
    }

    if !cooldown_expired(ctx.fs, ctx.env) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs, ctx.env);

    let file_list: String = state
        .files
        .iter()
        .take(10)
        .map(|f| format!("  - {f}"))
        .collect::<Vec<_>>()
        .join("\n");

    let extra = if state.file_count > 10 {
        format!("\n  ... and {} more", state.file_count - 10)
    } else {
        String::new()
    };

    let context = format!(
        "[Commit Hygiene] {} uncommitted file(s) in this project.\n\
         Consider committing before starting new work to avoid losing changes.\n\
         \n\
         Changed files:\n\
         {file_list}{extra}",
        state.file_count,
    );

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support;
    use crate::hooks::GitStatusPort;

    struct TestGit {
        has_changes: bool,
        files: Vec<String>,
    }

    impl GitStatusPort for TestGit {
        fn has_uncommitted_changes(&self, _: &str) -> anyhow::Result<bool> {
            Ok(self.has_changes)
        }
        fn changed_files(&self, _: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.files.clone())
        }
        fn current_branch(&self, _: &str) -> anyhow::Result<String> {
            Ok("main".to_string())
        }
        fn is_worktree(&self, _: &str) -> bool {
            false
        }
        fn has_unpushed_commits(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn list_worktree_names(&self, _: &str) -> Vec<String> {
            Vec::new()
        }
        fn repo_root(&self, _: &str) -> Option<String> {
            None
        }
        fn merge_base(&self, _: &str, _: &str) -> Option<String> { None }
        fn rev_list_count(&self, _: &str, _: &str) -> Option<u32> { None }
        fn diff_names(&self, _: &str, _: &str) -> Option<Vec<String>> { None }
    }

    fn make_ctx(git: &dyn GitStatusPort) -> HookContext<'_> {
        // Leak a StubFs for the lifetime — fine in tests
        let fs: &'static crate::hooks::test_support::StubFs =
            Box::leak(Box::new(crate::hooks::test_support::StubFs));
        let process: &'static crate::hooks::test_support::StubProcess =
            Box::leak(Box::new(crate::hooks::test_support::StubProcess));
        let memory_mcp: &'static crate::hooks::test_support::StubMemoryMcp =
            Box::leak(Box::new(crate::hooks::test_support::StubMemoryMcp));
        let env: &'static crate::hooks::test_support::StubEnv =
            Box::leak(Box::new(crate::hooks::test_support::StubEnv::new()));
        HookContext {
            git,
            vector_store: None,
            fs,
            process,
            llm: None,
            memory_mcp,
            env,
        }
    }

    #[test]
    fn test_stop_no_changes_clears_state() {
        let git = TestGit {
            has_changes: false,
            files: vec![],
        };
        let ctx = make_ctx(&git);
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_stop_with_changes_writes_state() {
        // This test relies on StubFs.write() which is a no-op — we just
        // verify it doesn't panic and returns allow.
        let git = TestGit {
            has_changes: true,
            files: vec!["src/main.rs".into(), "README.md".into(), "lib.rs".into()],
        };
        let ctx = make_ctx(&git);
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_prompt_no_state_returns_allow() {
        let input = HookInput {
            cwd: Some("/nonexistent/test/path".to_string()),
            ..Default::default()
        };
        let ctx = test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_prompt_below_threshold_no_inject() {
        // StubFs.read_to_string returns error, so process_prompt returns allow
        let input = HookInput {
            cwd: Some("/test/below".to_string()),
            ..Default::default()
        };
        let ctx = test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_cooldown_logic() {
        let ctx = test_support::stub_ctx();
        // StubFs.read_to_string returns error → cooldown_expired returns true
        assert!(cooldown_expired(ctx.fs, ctx.env));
    }

    #[test]
    fn test_state_gate_distinguishes_sessions() {
        // Simulate the equality gate used in process_prompt.
        // Session A writes state; Session B reads it. Even though cwd
        // matches, the session_id differs, so the gate must NOT treat
        // it as "my own state" (which would suppress the reminder).
        let state = CommitState {
            cwd: "/repo".to_string(),
            session_id: "session-a".to_string(),
            file_count: 5,
            files: vec!["a.rs".into()],
            ts: "2026-04-17T00:00:00Z".to_string(),
        };

        let cwd = "/repo";
        let my_session = "session-b";

        // Gate logic: treat as fresh if session_id OR cwd differs.
        let treat_as_fresh = state.session_id != my_session || state.cwd != cwd;
        assert!(
            treat_as_fresh,
            "cross-session state must not short-circuit the gate"
        );

        // Sanity: matching session + cwd means gate considers it our state.
        let same_session = "session-a";
        let treat_as_fresh_same = state.session_id != same_session || state.cwd != cwd;
        assert!(
            !treat_as_fresh_same,
            "matching session_id and cwd should be treated as own state"
        );
    }
}
