//! Commit Hygiene — Two-phase hook
//!
//! **Stop phase:** Detects uncommitted changes, writes state to
//! `~/.claude/metrics/commit-hygiene.json`.
//!
//! **UserPromptSubmit phase:** Reads state, checks cooldown (15 min),
//! injects reminder with file list.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::GitStatusPort;

/// Cooldown between commit reminders (15 minutes)
const COOLDOWN_MS: u64 = 15 * 60 * 1000;

/// Minimum files to trigger a reminder
const MIN_FILES: usize = 3;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CommitState {
    cwd: String,
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

fn state_file() -> Option<PathBuf> {
    // Allow tests to override the state file path to avoid race conditions
    // when multiple tests write/read the same file in parallel.
    if let Ok(path) = std::env::var("SENTINEL_COMMIT_HYGIENE_STATE") {
        return Some(PathBuf::from(path));
    }
    let home = dirs::home_dir()?;
    let dir = home.join(".claude").join("metrics");
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("commit-hygiene.json"))
}

fn cooldown_file() -> PathBuf {
    std::env::temp_dir().join("claude-commit-hygiene-last")
}

fn cooldown_expired() -> bool {
    let content = match fs::read_to_string(cooldown_file()) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown() {
    let _ = fs::write(cooldown_file(), now_ms().to_string());
}

// ---------------------------------------------------------------------------
// Stop phase: detect uncommitted changes and write state
// ---------------------------------------------------------------------------

pub fn process_stop(input: &HookInput, git: &dyn GitStatusPort) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let files = match git.has_uncommitted_changes(cwd) {
        Ok(true) => match git.changed_files(cwd) {
            Ok(f) if !f.is_empty() => f,
            _ => {
                // No changes — clear any previous state
                if let Some(path) = state_file() {
                    let _ = fs::remove_file(path);
                }
                return HookOutput::allow();
            }
        },
        _ => {
            if let Some(path) = state_file() {
                let _ = fs::remove_file(path);
            }
            return HookOutput::allow();
        }
    };

    let state = CommitState {
        cwd: cwd.to_string(),
        file_count: files.len(),
        files: files.into_iter().take(20).collect(), // Cap at 20 for readability
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = state_file() {
        let _ = fs::write(&path, serde_json::to_string(&state).unwrap_or_default());
    }

    tracing::debug!(count = state.file_count, "Uncommitted changes detected");
    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject commit reminder
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput) -> HookOutput {
    let cwd = input.cwd.as_deref().unwrap_or(".");

    let path = match state_file() {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: CommitState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Only remind for the current project
    if state.cwd != cwd {
        return HookOutput::allow();
    }

    // Don't nag for small change sets
    if state.file_count < MIN_FILES {
        return HookOutput::allow();
    }

    if !cooldown_expired() {
        return HookOutput::allow();
    }

    write_cooldown();

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

    struct StubGit {
        has_changes: bool,
        files: Vec<String>,
    }

    impl GitStatusPort for StubGit {
        fn has_uncommitted_changes(&self, _repo_path: &str) -> anyhow::Result<bool> {
            Ok(self.has_changes)
        }
        fn changed_files(&self, _repo_path: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.files.clone())
        }
    }

    // Mutex to serialize tests that use env var overrides for the state file.
    // set_var/remove_var are process-global and unsafe in parallel tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_stop_no_changes_clears_state() {
        let git = StubGit {
            has_changes: false,
            files: vec![],
        };
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process_stop(&input, &git);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_stop_with_changes_writes_state() {
        let _guard = ENV_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "commit-hygiene-test-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::env::set_var("SENTINEL_COMMIT_HYGIENE_STATE", tmp.to_str().unwrap());
        let _ = fs::remove_file(&tmp);

        let git = StubGit {
            has_changes: true,
            files: vec!["src/main.rs".into(), "README.md".into(), "lib.rs".into()],
        };
        let input = HookInput {
            cwd: Some(".".to_string()),
            ..Default::default()
        };
        let output = process_stop(&input, &git);
        assert!(output.blocked.is_none());

        assert!(tmp.exists(), "State file should have been written");
        let state: CommitState = serde_json::from_str(&fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(state.file_count, 3);

        let _ = fs::remove_file(&tmp);
        std::env::remove_var("SENTINEL_COMMIT_HYGIENE_STATE");
    }

    #[test]
    fn test_prompt_no_state_returns_allow() {
        let input = HookInput {
            cwd: Some("/nonexistent/test/path".to_string()),
            ..Default::default()
        };
        let output = process_prompt(&input);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_prompt_below_threshold_no_inject() {
        let _guard = ENV_LOCK.lock().unwrap();

        let tmp = std::env::temp_dir().join(format!(
            "commit-hygiene-below-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        std::env::set_var("SENTINEL_COMMIT_HYGIENE_STATE", tmp.to_str().unwrap());

        let state = CommitState {
            cwd: "/test/below".to_string(),
            file_count: 2,
            files: vec!["a.rs".into(), "b.rs".into()],
            ts: "2026-03-05".into(),
        };
        let _ = fs::write(&tmp, serde_json::to_string(&state).unwrap());

        let input = HookInput {
            cwd: Some("/test/below".to_string()),
            ..Default::default()
        };
        let output = process_prompt(&input);
        assert!(output.hook_specific_output.is_none());

        let _ = fs::remove_file(&tmp);
        std::env::remove_var("SENTINEL_COMMIT_HYGIENE_STATE");
    }

    #[test]
    fn test_cooldown_logic() {
        let _ = fs::remove_file(cooldown_file());
        assert!(cooldown_expired());

        write_cooldown();
        assert!(!cooldown_expired());

        // Clean up
        let _ = fs::remove_file(cooldown_file());
    }
}
