//! Pre-Push Steel Test Hook
//!
//! Blocks `git push` commands when a Steel browser test hasn't been run
//! in the current session. Ensures UI changes are browser-verified before
//! code reaches the remote.
//!
//! Session state tracked via temp file: {tmpdir}/claude-steel-test-{session_id}.json
//! State format: {"passed": true, "sessionId": "...", "timestamp": "ISO8601"}
//!
//! If no project config has Steel test settings, the push is allowed.

use chrono::Utc;
use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;
use std::time::Duration;

/// Steel test validity window (2 hours)
const TEST_VALIDITY: Duration = Duration::from_secs(2 * 60 * 60);

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

    // Get session ID
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    // Check if Steel test passed recently
    if has_recent_steel_test(session_id) {
        return HookOutput::allow();
    }

    // Check if any project has Steel test config
    if !any_project_has_steel_config() {
        return HookOutput::allow();
    }

    // Block — Steel test not run
    let message = "\
+============================================================+
|  BLOCKED: Steel Live Test Required Before Push             |
+============================================================+
|  A Steel browser test must pass before pushing.            |
|                                                            |
|  Run the test first:                                       |
|  -> Local: node scripts/steel-live-test.js --local         |
|  -> Staging: node scripts/steel-live-test.js --staging     |
|                                                            |
|  Or mark test as passed:                                   |
|  -> Say \"steel test passed\" to bypass                      |
|  -> Say \"override push\" to push anyway                     |
+============================================================+
|  This ensures UI changes are browser-verified before       |
|  reaching the remote repository.                           |
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
}
