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
//! 2. Matches cwd repo name against project configs with Steel settings
//! 3. If current repo has no matching Steel-configured project → allow
//! 4. Checks if diff includes frontend files (.tsx, .jsx, .css, .scss, .styled)
//! 5. If no frontend files → allow (backend-only push)
//! 6. If frontend files + recent Steel test → allow
//! 7. If frontend files + no Steel test → block with instructions

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
/// (Public wrapper for CLI access)
pub fn has_recent_steel_test_pub(session_id: &str) -> bool {
    has_recent_steel_test(session_id)
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

/// Extract the repo directory name from a cwd path.
/// e.g. "C:\Users\garys\Documents\GitHub\firefly-pro-crm" → "firefly-pro-crm"
/// Also handles worktree paths like "repo--branch-name" by stripping the "--" suffix.
fn repo_name_from_cwd(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    let name = path.file_name()?.to_string_lossy().to_string();
    // Strip worktree suffix (e.g. "firefly-pro-crm--fir-123-desc" → "firefly-pro-crm")
    let base = name.split("--").next().unwrap_or(&name);
    Some(base.to_lowercase())
}

/// Check if the current repo matches a project config that has Steel test settings.
/// Scoped check: only returns true if the repo name matches the project's name or aliases
/// AND that project has steel_test_email configured.
///
/// Accepts an optional override path for testing; uses ~/.claude/skills/linear/projects/ by default.
fn repo_has_steel_config_in(
    cwd: Option<&str>,
    projects_dir: Option<&std::path::Path>,
) -> bool {
    let repo = match cwd.and_then(repo_name_from_cwd) {
        Some(r) => r,
        None => return false, // No cwd → can't determine repo → allow
    };

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
            // Must have steel_test_email to be a Steel-configured project
            if !content.contains("steel_test_email") {
                continue;
            }

            // Check if repo name matches project name or aliases
            let content_lower = content.to_lowercase();
            if repo_matches_project(&repo, &content_lower) {
                return true;
            }
        }
    }

    false
}

/// Check if a repo name matches a project config's name or aliases.
/// Matches against:
/// - `name:` frontmatter field (e.g. "name: firefly-pro")
/// - `aliases:` frontmatter array (e.g. aliases: ["firefly", "crm", "fpcrm"])
/// - The filename stem of the project file
///
/// Repo names are matched with normalization: "firefly-pro-crm" matches alias "fpcrm",
/// name "firefly-pro", and common variants like "firefly" by checking if the repo
/// name contains or is contained by any alias.
fn repo_matches_project(repo: &str, content_lower: &str) -> bool {
    // Extract name field: `name: firefly-pro`
    for line in content_lower.lines() {
        let trimmed = line.trim();
        if let Some(name_val) = trimmed.strip_prefix("name:") {
            let name = name_val.trim().trim_matches('"');
            if repo.contains(name) || name.contains(repo) {
                return true;
            }
        }

        // Extract aliases array: `aliases: ["firefly", "crm", "fpcrm"]`
        if let Some(aliases_val) = trimmed.strip_prefix("aliases:") {
            let aliases_str = aliases_val.trim();
            // Parse simple array format: ["a", "b", "c"]
            let cleaned = aliases_str
                .trim_start_matches('[')
                .trim_end_matches(']');
            for alias in cleaned.split(',') {
                let alias = alias.trim().trim_matches('"').trim_matches('\'');
                if alias.is_empty() {
                    continue;
                }
                if repo.contains(alias) || alias.contains(repo) {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if the current repo has Steel test config (default projects path)
fn repo_has_steel_config(cwd: Option<&str>) -> bool {
    repo_has_steel_config_in(cwd, None)
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

/// Write the Steel test state file after a successful Steel session.
/// Called from the PostToolUse handler when `mcp__steel__release_session` succeeds.
pub fn record_steel_test_passed(session_id: &str) {
    let path = state_file_path(session_id);
    let state = serde_json::json!({
        "passed": true,
        "sessionId": session_id,
        "timestamp": Utc::now().to_rfc3339()
    });
    if let Err(e) = std::fs::write(&path, serde_json::to_string(&state).unwrap_or_default()) {
        tracing::warn!("Failed to write Steel test state file: {e}");
    } else {
        tracing::debug!("Steel test state recorded at {}", path.display());
    }
}

/// PostToolUse handler — detect successful browser tests and record test state.
/// Triggers on:
/// 1. `mcp__steel__release_session` — Steel MCP test completed
/// 2. Bash tool result containing `STEEL_TEST_PASS` — CDP/Puppeteer test completed
///
/// Should be called from the PostToolUse event dispatch in hook_cmd.rs.
pub fn process_post_tool(input: &HookInput) -> HookOutput {
    let tool = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return HookOutput::allow(),
    };

    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    // Path 1: Steel MCP release_session
    if tool == "mcp__steel__release_session" {
        record_steel_test_passed(session_id);
        return HookOutput::allow();
    }

    // Path 2: Bash tool with STEEL_TEST_PASS marker in output
    // This supports CDP, Puppeteer, Playwright, or any browser test that
    // prints "STEEL_TEST_PASS" on success.
    if tool == "Bash" {
        let has_marker = input
            .tool_result
            .as_ref()
            .and_then(|r| r.as_str())
            .map_or(false, |s| s.contains("STEEL_TEST_PASS"));
        if has_marker {
            record_steel_test_passed(session_id);
        }
    }

    HookOutput::allow()
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

    // Check if THIS repo's project has Steel test config (not all projects globally)
    let cwd = input.cwd.as_deref();
    if !repo_has_steel_config(cwd) {
        return HookOutput::allow();
    }

    // Check if the diff includes frontend files
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
|  Or push manually from your terminal:                      |
|  -> git push origin <branch>                               |
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
        let result = repo_has_steel_config_in(Some("/fake/path/some-repo"), Some(tmpdir.path()));
        assert!(!result, "Empty directory should have no steel config");
    }

    #[test]
    fn test_detects_steel_config_for_matching_repo() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("firefly.md");
        std::fs::write(
            &project_file,
            "name: firefly-pro\naliases: [\"firefly\", \"crm\", \"fpcrm\"]\nsteel_test_email: test@example.com",
        )
        .unwrap();
        // Repo name "firefly-pro-crm" contains alias "crm" → match
        let result = repo_has_steel_config_in(
            Some("/fake/path/firefly-pro-crm"),
            Some(tmpdir.path()),
        );
        assert!(result, "Should match repo 'firefly-pro-crm' against alias 'crm'");
    }

    #[test]
    fn test_ignores_steel_config_for_unrelated_repo() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("firefly.md");
        std::fs::write(
            &project_file,
            "name: firefly-pro\naliases: [\"firefly\", \"crm\", \"fpcrm\"]\nsteel_test_email: test@example.com",
        )
        .unwrap();
        // Repo name "sentinel" doesn't match any alias → no block
        let result = repo_has_steel_config_in(
            Some("/fake/path/sentinel"),
            Some(tmpdir.path()),
        );
        assert!(!result, "Should NOT match repo 'sentinel' against firefly aliases");
    }

    #[test]
    fn test_ignores_project_without_steel_email() {
        let tmpdir = tempfile::tempdir().unwrap();
        let project_file = tmpdir.path().join("myproject.md");
        // Has staging_url but NOT steel_test_email → should not trigger Steel gate
        std::fs::write(
            &project_file,
            "name: myproject\naliases: [\"myapp\"]\nstaging_url: https://staging.example.com",
        )
        .unwrap();
        let result = repo_has_steel_config_in(
            Some("/fake/path/myproject"),
            Some(tmpdir.path()),
        );
        assert!(!result, "Should NOT match project without steel_test_email");
    }

    #[test]
    fn test_worktree_path_strips_branch_suffix() {
        assert_eq!(
            repo_name_from_cwd("/path/to/firefly-pro-crm--fir-123-desc"),
            Some("firefly-pro-crm".to_string())
        );
        assert_eq!(
            repo_name_from_cwd("/path/to/sentinel"),
            Some("sentinel".to_string())
        );
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

    #[test]
    fn test_record_steel_test_passed_writes_state_file() {
        let session_id = "test-record-steel";
        let state_path = state_file_path(session_id);

        // Ensure clean state
        let _ = std::fs::remove_file(&state_path);

        record_steel_test_passed(session_id);

        assert!(state_path.exists(), "State file should be created");
        let content = std::fs::read_to_string(&state_path).unwrap();
        let state: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(state["passed"], true);
        assert_eq!(state["sessionId"], session_id);
        assert!(state["timestamp"].is_string());

        // Verify it's recognized as a recent test
        assert!(has_recent_steel_test(session_id));

        // Cleanup
        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_process_post_tool_records_on_release() {
        let session_id = "test-post-tool-release";
        let state_path = state_file_path(session_id);
        let _ = std::fs::remove_file(&state_path);

        // Claude Code does NOT populate tool_result for MCP tools —
        // PostToolUse firing is sufficient proof the call succeeded
        let input = HookInput {
            tool_name: Some("mcp__steel__release_session".to_string()),
            tool_result: None,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let output = process_post_tool(&input);
        assert!(output.blocked.is_none());
        assert!(has_recent_steel_test(session_id), "State file should be written after release");

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_process_post_tool_ignores_bash_without_marker() {
        let session_id = "test-post-tool-no-marker";
        let state_path = state_file_path(session_id);
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_result: Some(serde_json::json!("ok")),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let output = process_post_tool(&input);
        assert!(output.blocked.is_none());
        assert!(!state_path.exists(), "State file should NOT be created for Bash without STEEL_TEST_PASS");
    }

    #[test]
    fn test_process_post_tool_records_on_cdp_marker() {
        let session_id = "test-post-tool-cdp";
        let state_path = state_file_path(session_id);
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_result: Some(serde_json::json!("Screenshot saved: C:\\tmp\\screenshot.png\nConsole errors: 0\n  No console errors detected\nSTEEL_TEST_PASS")),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let output = process_post_tool(&input);
        assert!(output.blocked.is_none());
        assert!(has_recent_steel_test(session_id), "State file should be written after CDP test with STEEL_TEST_PASS marker");

        let _ = std::fs::remove_file(&state_path);
    }

    #[test]
    fn test_process_post_tool_ignores_non_bash_non_steel() {
        let session_id = "test-post-tool-read";
        let state_path = state_file_path(session_id);
        let _ = std::fs::remove_file(&state_path);

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_result: Some(serde_json::json!("STEEL_TEST_PASS")),
            session_id: Some(session_id.to_string()),
            ..Default::default()
        };
        let output = process_post_tool(&input);
        assert!(output.blocked.is_none());
        assert!(!state_path.exists(), "State file should NOT be created for Read tool even with marker");
    }
}
