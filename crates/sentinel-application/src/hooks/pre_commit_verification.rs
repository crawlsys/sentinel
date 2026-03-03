//! Pre-Commit Verification Gate
//!
//! Blocks `git commit` and `git push` unless test/build evidence exists
//! in the session transcript.
//!
//! Two-layer verification (ported from Node.js pre-commit-verification.js):
//!   Layer 1 (regex): Did any tests/builds run? (fast, ~0ms)
//!   Layer 2 (AI):    Skipped for now — regex-only detection
//!
//! Override: temp file at {tmpdir}/claude-verification-override (5 min TTL)

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Override file validity window (5 minutes)
const OVERRIDE_TIMEOUT: Duration = Duration::from_secs(300);

/// Test command patterns that count as verification evidence
const TEST_COMMAND_PATTERNS: &[&str] = &[
    r"\bnpm\s+test\b",
    r"\bnpx\s+(vitest|jest|mocha|cypress)\b",
    r"\byarn\s+test\b",
    r"\bpnpm\s+test\b",
    r"\bcargo\s+test\b",
    r"\bpytest\b",
    r"\bgo\s+test\b",
    r"\bnpm\s+run\s+(test|build|lint|check|typecheck)\b",
    r"\btsc\b.*--noEmit",
    r"\bvitest\b",
    r"\bjest\b",
    r"\bmake\s+test\b",
    r"\bnpm\s+run\s+build\b",
];

/// Test output patterns that confirm tests ran
const TEST_OUTPUT_PATTERNS: &[&str] = &[
    r"\d+\s+pass(?:ing|ed)?",
    r"\d+\s+fail(?:ing|ed)?",
    r"exit code:?\s*0",
    r"tests?\s+suites?.*passed",
    r"PASS",
    r"BUILD SUCCESS",
    r"Successfully compiled",
    r"All \d+ tests? passed",
];

/// Check if the override temp file exists and is still valid
fn is_override_active_at(path: &std::path::Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if let Ok(modified) = meta.modified() {
                if let Ok(elapsed) = SystemTime::now().duration_since(modified) {
                    if elapsed < OVERRIDE_TIMEOUT {
                        return true;
                    }
                    // Expired — try to clean up
                    let _ = std::fs::remove_file(path);
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Path to the default override temp file
fn default_override_path() -> PathBuf {
    std::env::temp_dir().join("claude-verification-override")
}

/// Check the transcript for test evidence (Layer 1: regex)
fn transcript_has_test_evidence(transcript_path: &str) -> bool {
    let content = match std::fs::read_to_string(transcript_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Build regex patterns
    let cmd_patterns: Vec<Regex> = TEST_COMMAND_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    let output_patterns: Vec<Regex> = TEST_OUTPUT_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    // Check each line of the transcript
    for line in content.lines() {
        // Try to parse as JSON transcript entry
        if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
            // Check assistant messages for Bash tool_use with test commands
            if entry.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                if let Some(content_arr) = entry
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content_arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                            && block.get("name").and_then(|n| n.as_str()) == Some("Bash")
                        {
                            let cmd = block
                                .get("input")
                                .and_then(|i| i.get("command"))
                                .and_then(|c| c.as_str())
                                .unwrap_or("");
                            for pattern in &cmd_patterns {
                                if pattern.is_match(cmd) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }

            // Check tool results for test output
            if entry.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                let text = entry
                    .get("content")
                    .map(|c| {
                        if let Some(s) = c.as_str() {
                            s.to_string()
                        } else if let Some(arr) = c.as_array() {
                            arr.iter()
                                .filter_map(|item| {
                                    item.as_str()
                                        .map(String::from)
                                        .or_else(|| item.get("text").and_then(|t| t.as_str()).map(String::from))
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default();

                for pattern in &output_patterns {
                    if pattern.is_match(&text) {
                        return true;
                    }
                }
            }
        }

        // Also do a raw line check for common patterns (handles non-JSON transcript lines)
        for pattern in &cmd_patterns {
            if pattern.is_match(line) {
                return true;
            }
        }
        for pattern in &output_patterns {
            if pattern.is_match(line) {
                return true;
            }
        }
    }

    false
}

/// Process a pre-commit verification hook event (PreToolUse).
/// Uses the default override path at `{tmpdir}/claude-verification-override`.
pub fn process(input: &HookInput) -> HookOutput {
    process_with_override(input, &default_override_path())
}

/// Internal: process with an explicit override file path (for testability).
fn process_with_override(input: &HookInput, override_path: &std::path::Path) -> HookOutput {
    // Only act on Bash tool calls
    let tool = match &input.tool_name {
        Some(name) if name == "Bash" => name.as_str(),
        _ => return HookOutput::allow(),
    };
    let _ = tool;

    // Extract command from tool_input
    let command = input
        .tool_input
        .as_ref()
        .and_then(|ti| ti.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    // Check if this is a git commit or git push
    let git_re = match Regex::new(r"\bgit\s+(commit|push)\b") {
        Ok(re) => re,
        Err(_) => return HookOutput::allow(),
    };

    let caps = match git_re.captures(command) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    let action = caps.get(1).map_or("commit", |m| m.as_str());
    let action_gerund = if action == "commit" {
        "Committing"
    } else {
        "Pushing"
    };

    // Check override temp file
    if is_override_active_at(override_path) {
        return HookOutput::allow();
    }

    // Layer 1: Check transcript for test evidence
    if let Some(ref transcript_path) = input.transcript_path {
        if transcript_has_test_evidence(transcript_path) {
            return HookOutput::allow();
        }
    }

    // No evidence found — BLOCK
    let message = format!(
        "\
+============================================================+
|  BLOCKED: Run Tests Before {action_gerund:<34}|
+============================================================+
|  No test/build evidence found in this session.             |
|                                                            |
|  Run verification first:                                   |
|  -> npm test / vitest / jest / cargo test                  |
|  -> npm run build / tsc --noEmit                           |
|  -> npm run lint / npm run check                           |
|                                                            |
|  Or bypass:                                                |
|  -> Say \"override verification\" (5 min window)             |
+============================================================+
|  Evidence before claims. Always.                           |
+============================================================+"
    );

    HookOutput::block(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_allows_non_bash_tool() {
        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({"file_path": "foo.rs"})),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_non_git_bash_command() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_git_commit_without_evidence() {
        // Use a non-existent override path to isolate from parallel tests
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'test'"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("BLOCKED"));
        assert!(output.reason.as_ref().unwrap().contains("Committing"));
    }

    #[test]
    fn test_blocks_git_push_without_evidence() {
        // Use a non-existent override path to isolate from parallel tests
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("Pushing"));
    }

    #[test]
    fn test_allows_when_transcript_has_evidence() {
        // Create a temp transcript with test evidence
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmpfile,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"npm test"}}}}]}}}}"#
        )
        .unwrap();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'tested'"})),
            transcript_path: Some(tmpfile.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_when_override_active() {
        // Use an isolated override file to avoid race conditions
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("test-override");

        // No override file — should not be active
        assert!(!is_override_active_at(&override_path));

        // Write the override file
        std::fs::write(&override_path, "override").unwrap();
        assert!(is_override_active_at(&override_path));

        // Verify process respects it
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'override'"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_transcript_output_patterns_detected() {
        let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmpfile,
            r#"{{"type":"tool_result","content":"5 passing (200ms)"}}"#
        )
        .unwrap();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            transcript_path: Some(tmpfile.path().to_string_lossy().to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_git_commit_amend() {
        // Use a non-existent override path to isolate from parallel tests
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit --amend"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path);
        assert_eq!(output.blocked, Some(true));
    }
}
