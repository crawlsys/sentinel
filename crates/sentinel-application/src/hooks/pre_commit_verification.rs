//! Pre-Commit Verification Gate
//!
//! Blocks `git commit` and `git push` unless test/build evidence exists
//! in the session transcript.
//!
//! Two-layer verification (ported from Node.js pre-commit-verification.js):
//!   Layer 1 (regex): Did any tests/builds run? (fast, ~0ms)
//!   Layer 2 (AI):    Skipped for now — regex-only detection
//!
//! Override: session-scoped temp file (60s TTL, via hygiene_override module)

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;

/// Test command patterns that count as verification evidence
const TEST_COMMAND_PATTERNS: &[&str] = &[
    r"\bnpm\s+test\b",
    r"\bnpx\s+(vitest|jest|mocha|cypress)\b",
    r"\byarn\s+test\b",
    r"\bpnpm\s+test\b",
    r"\bcargo\s+test\b",
    r"\bcargo\s+build\b",
    r"\bpytest\b",
    r"\bgo\s+test\b",
    r"\bnpm\s+run\s+(test|build|lint|check|typecheck)\b",
    r"\btsc\b.*--noEmit",
    r"\bvitest\b",
    r"\bjest\b",
    r"\bmake\s+test\b",
    r"\bmake\s+build\b",
    r"\bnpm\s+run\s+build\b",
    r"\bdocker\s+build\b",
    r"\bdepot\s+build\b",
    r"\bdepot\s+list\s+builds\b",
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

/// Path to the default override file (session-scoped via hygiene_override).
fn default_override_path(session_id: &str) -> PathBuf {
    super::hygiene_override::verification_override_path(session_id)
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
                                    item.as_str().map(String::from).or_else(|| {
                                        item.get("text").and_then(|t| t.as_str()).map(String::from)
                                    })
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

        // **Attack #59 fix**: Removed raw line fallback. Only structured JSON entries
        // are trusted for test evidence. Raw text lines could be injected by an
        // attacker (e.g., `echo "Tests: 5 passed" >> transcript.jsonl`) to fake
        // passing test evidence without actually running tests.
    }

    false
}

/// Process a pre-commit verification hook event (PreToolUse).
/// Uses session-scoped signed override check.
pub fn process(input: &HookInput) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let override_path = default_override_path(session_id);
    process_with_override(input, &override_path, session_id)
}

/// Internal: process with an explicit override file path (for testability).
fn process_with_override(
    input: &HookInput,
    override_path: &std::path::Path,
    session_id: &str,
) -> HookOutput {
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

    // Check signed override file (Attack #47: replaces mtime-only check)
    if super::hygiene_override::is_signed_override_active(override_path, "verification", session_id)
    {
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

    HookOutput::block(super::block_context::append_block_context(message, input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::hygiene_override;
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
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'test'"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path, "test-sess");
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("BLOCKED"));
        assert!(output.reason.as_ref().unwrap().contains("Committing"));
    }

    #[test]
    fn test_blocks_git_push_without_evidence() {
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("no-override");

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git push origin main"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path, "test-sess");
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
    fn test_allows_when_signed_override_active() {
        let tmpdir = tempfile::tempdir().unwrap();
        let override_path = tmpdir.path().join("test-override");
        let session_id = "test-sess-override";

        // No override file — should not be active
        assert!(!hygiene_override::is_signed_override_active(
            &override_path,
            "verification",
            session_id
        ));

        // Write a properly signed override file
        hygiene_override::write_signed_override_for_test(
            &override_path,
            "verification",
            session_id,
        );
        assert!(hygiene_override::is_signed_override_active(
            &override_path,
            "verification",
            session_id
        ));

        // Verify process respects it
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "git commit -m 'override'"})),
            ..Default::default()
        };
        let output = process_with_override(&input, &override_path, session_id);
        assert!(output.blocked.is_none());

        // Verify that a plain `touch` doesn't work
        let touch_path = tmpdir.path().join("touch-override");
        std::fs::write(&touch_path, "").unwrap();
        assert!(!hygiene_override::is_signed_override_active(
            &touch_path,
            "verification",
            session_id
        ));
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
        let output = process_with_override(&input, &override_path, "test-sess");
        assert_eq!(output.blocked, Some(true));
    }
}
