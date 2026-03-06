//! Pre-Push Steel Test Hook
//!
//! Blocks `git push` commands when a Steel browser test hasn't been run
//! in the current session AND the push includes frontend file changes.
//! Ensures UI changes are browser-verified before code reaches the remote.
//!
//! This is the safety net behind Layer 2.5 (pre-push local Steel test) in
//! the Linear review phase. If the skill-level gate was followed, the state
//! file will already exist and this hook allows the push instantly.
//!
//! Session state tracked via temp file: {tmpdir}/claude-steel-test-{session_id}.json
//! State format: {"passed": true, "sessionId": "...", "timestamp": "ISO8601"}
//!
//! Logic:
//! 1. Only fires on `git push` commands
//! 2. Checks if diff includes frontend files (.tsx, .jsx, .css, .scss, .styled)
//! 3. If no frontend files → allow (backend-only push)
//! 4. If frontend files + recent Steel test → allow
//! 5. If frontend files + no Steel test → block with instructions
//! 6. If no project has Steel config → allow (Steel not configured)

use chrono::Utc;
use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;
use std::time::Duration;

/// Steel test validity window (2 hours)
const TEST_VALIDITY: Duration = Duration::from_secs(2 * 60 * 60);

/// Frontend file extensions that trigger Steel test requirement
const FRONTEND_EXTENSIONS: &[&str] = &[".tsx", ".jsx", ".css", ".scss", ".styled"];

/// Path to the Steel test state file for a given session
fn state_file_path(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-steel-test-{session_id}.json"))
}

/// Check if a passing Steel test exists for this session within the validity window
fn has_recent_steel_test(session_id: &str) -> bool {
    let path = state_file_path(session_id);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let state: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Verify passed flag and session match
    let passed = state.get("passed").and_then(|v| v.as_bool()).unwrap_or(false);
    let state_session = state
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !passed || state_session != session_id {
        return false;
    }

    // Check timestamp is within validity window
    let timestamp = match state.get("timestamp").and_then(|v| v.as_str()) {
        Some(ts) => ts,
        None => return false,
    };

    match chrono::DateTime::parse_from_rfc3339(timestamp) {
        Ok(test_time) => {
            let elapsed = Utc::now().signed_duration_since(test_time);
            elapsed.num_seconds() >= 0
                && elapsed.to_std().map_or(false, |d| d < TEST_VALIDITY)
        }
        Err(_) => false,
    }
}

/// Check if any project config has Steel test settings.
/// Accepts an optional override path for testing; uses ~/.claude/skills/linear/projects/ by default.
fn any_project_has_steel_config_in(projects_dir: Option<&std::path::Path>) -> bool {
    let default_dir = dirs::home_dir()
        .map(|h| h.join(".claude").join("skills").join("linear").join("projects"));

    let projects_dir = match projects_dir {
        Some(d) => d.to_path_buf(),
        None => match default_dir {
            Some(d) if d.is_dir() => d,
            _ => return false,
        },
    };

    if !projects_dir.is_dir() {
        return false;
    }

    let entries = match std::fs::read_dir(&projects_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map_or(true, |e| e != "md") {
            continue;
        }
        if path
            .file_name()
            .map_or(false, |n| n.to_string_lossy().starts_with('_'))
        {
            continue;
        }

        if let Ok(content) = std::fs::read_to_string(&path) {
            if content.contains("steel_test_email") || content.contains("staging_url") {
                return true;
            }
        }
    }

    false
}

/// Check if any project config has Steel test settings (default path)
fn any_project_has_steel_config() -> bool {
    any_project_has_steel_config_in(None)
}

/// Check if the git diff (staged or branch) includes frontend file changes.
/// Uses the working directory from the hook input.
fn diff_has_frontend_files(cwd: Option<&str>) -> bool {
    let dir = cwd.unwrap_or(".");

    // Try to get the diff stat against the tracking branch
    let output = std::process::Command::new("git")
        .args(["diff", "--name-only", "@{upstream}..HEAD"])
        .current_dir(dir)
        .output();

    // Fallback: diff against origin/main if no upstream
    let output = match output {
        Ok(ref o) if o.status.success() && !o.stdout.is_empty() => output,
        _ => std::process::Command::new("git")
            .args(["diff", "--name-only", "origin/main..HEAD"])
            .current_dir(dir)
            .output(),
    };

    let file_list = match output {
        Ok(ref o) if o.status.success() => String::from_utf8_lossy(&o.stdout),
        _ => return false, // Can't determine diff — allow push
    };

    file_list
        .lines()
        .any(|line| FRONTEND_EXTENSIONS.iter().any(|ext| line.ends_with(ext)))
}

/// Process a pre-push Steel test hook event (PreToolUse)
pub fn process(input: &HookInput) -> HookOutput {
    // Only act on Bash tool calls
    let tool = match &input.tool_name {
        Some(name) if name == "Bash" => name.as_str(),
        _ => return HookOutput::allow(),
    };
    let _ = tool;

    // Extract command
    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    // Check if this is a git push
    let push_re = match Regex::new(r"\bgit\s+push\b") {
        Ok(re) => re,
        Err(_) => return HookOutput::allow(),
    };

    if !push_re.is_match(command) {
        return HookOutput::allow();
    }

    // Check if any project has Steel test config
    if !any_project_has_steel_config() {
        return HookOutput::allow();
    }

    // Check if the diff includes frontend files
    let cwd = input.cwd.as_deref();
    if !diff_has_frontend_files(cwd) {
        // Backend-only change — no Steel test needed
        return HookOutput::allow();
    }

    // Get session ID
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    // Check if Steel test passed recently
    if has_recent_steel_test(session_id) {
        return HookOutput::allow();
    }

    // Block — frontend files changed but no Steel test run
    let message = "\
+============================================================+
|  BLOCKED: Steel Test Required — Frontend Changes Detected  |
+============================================================+
|  Your push includes frontend file changes (.tsx/.jsx/.css) |
|  but no Steel browser test has been run this session.      |
|                                                            |
|  Run Layer 2.5 (Pre-Push Local Steel Test) first:          |
|  1. Start local dev server                                 |
|  2. Start cloudflared tunnel                               |
|  3. Create Steel session → login → screenshot → verify     |
|  4. Check console errors                                   |
|                                                            |
|  Or mark test as passed:                                   |
|  -> Say \"steel test passed\" to bypass                      |
|  -> Say \"override push\" to push anyway                     |
+============================================================+"
        .to_string();

    HookOutput::block(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    #[test]
    fn test_allows_non_bash_tool() {
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_non_push_command() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'test'"})),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_push_when_no_steel_config() {
        // Use an empty temp dir — no project config files with steel settings
        let tmpdir = tempfile::tempdir().unwrap();
        let result = any_project_has_steel_config_in(Some(tmpdir.path()));
        assert!(!result, "Empty directory should have no steel config");
    }

    #[test]
    fn test_detects_steel_config_in_project_files() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("firefly.md");
        std::fs::write(&project_file, "staging_url: https://staging.example.com").unwrap();
        let result = any_project_has_steel_config_in(Some(tmpdir.path()));
        assert!(result, "Should detect staging_url in project file");
    }

    #[test]
    fn test_allows_push_with_recent_steel_test() {
        let session_id = "test-steel-recent";
        let state_path = state_file_path(session_id);

        // Write a valid recent state file
        let state = serde_json::json!({
            "passed": true,
            "sessionId": session_id,
            "timestamp": Utc::now().to_rfc3339()
        });
        let mut file = std::fs::File::create(&state_path).unwrap();
        write!(file, "{}", serde_json::to_string(&state).unwrap()).unwrap();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());

        // Cleanup
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_expired_steel_test_not_valid() {
        let session_id = "test-steel-expired";
        let result = has_recent_steel_test(session_id);
        assert!(!result);
    }

    #[test]
    fn test_mismatched_session_not_valid() {
        let session_id = "test-steel-mismatch";
        let state_path = state_file_path(session_id);

        let state = serde_json::json!({
            "passed": true,
            "sessionId": "different-session",
            "timestamp": Utc::now().to_rfc3339()
        });
        std::fs::write(&state_path, serde_json::to_string(&state).unwrap()).unwrap();

        let result = has_recent_steel_test(session_id);
        assert!(!result);

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_frontend_extensions_list() {
        // Verify our extension list covers the expected files
        assert!(FRONTEND_EXTENSIONS.contains(&".tsx"));
        assert!(FRONTEND_EXTENSIONS.contains(&".jsx"));
        assert!(FRONTEND_EXTENSIONS.contains(&".css"));
        assert!(FRONTEND_EXTENSIONS.contains(&".scss"));
        assert!(FRONTEND_EXTENSIONS.contains(&".styled"));
    }

    #[test]
    fn test_diff_has_frontend_files_non_git_dir() {
        // Non-git directory should return false (allow push)
        let tmpdir = tempfile::tempdir().unwrap();
        let result = diff_has_frontend_files(Some(tmpdir.path().to_str().unwrap()));
        assert!(!result, "Non-git dir should return false (allow push)");
    }
}
