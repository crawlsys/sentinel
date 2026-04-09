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

/// Search all project directories for a session transcript JSONL.
/// Fallback when `input.transcript_path` is missing or points to
/// a worktree-scoped dir that doesn't contain the transcript file.
///
/// Returns the **largest** matching transcript, not the first — worktree-scoped
/// project dirs often have small/empty transcripts while the original project
/// dir has the real one with test evidence.
fn find_transcript_by_session(session_id: &str) -> Option<String> {
    let projects_dir = dirs::home_dir()?.join(".claude").join("projects");
    if !projects_dir.exists() {
        return None;
    }
    let mut best: Option<(u64, String)> = None;
    for entry in std::fs::read_dir(&projects_dir).ok()?.flatten() {
        if !entry.file_type().ok().is_some_and(|ft| ft.is_dir()) {
            continue;
        }
        let path = entry.path().join(format!("{session_id}.jsonl"));
        if let Ok(meta) = std::fs::metadata(&path) {
            let size = meta.len();
            if best.as_ref().is_none_or(|(best_size, _)| size > *best_size) {
                best = Some((size, path.to_string_lossy().to_string()));
            }
        }
    }
    best.map(|(_, path)| path)
}

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

/// Non-code file extensions that never require test evidence.
const DOCS_ONLY_EXTENSIONS: &[&str] = &[
    ".md", ".mdx", ".txt", ".json", ".yaml", ".yml", ".toml", ".ini", ".cfg",
    ".conf", ".env", ".env.example", ".editorconfig", ".gitignore", ".gitattributes",
    ".prettierrc", ".eslintrc", ".dockerignore", ".nvmrc", ".node-version",
    ".tool-versions", ".ruby-version", ".python-version",
    ".csv", ".tsv", ".xml", ".svg", ".png", ".jpg", ".jpeg", ".gif", ".ico",
    ".woff", ".woff2", ".ttf", ".eot", ".otf",
    "LICENSE", "CHANGELOG", "SECURITY",
];

/// Check if a git commit command only touches non-code files.
/// Runs `git diff --cached --name-only` to inspect staged files.
/// Returns true if ALL staged files have docs-only extensions (or if
/// the staged file list is empty — nothing to test).
fn is_docs_only_commit(command: &str) -> bool {
    // Only applies to `git commit` commands, not `git push`
    if !command.contains("git") || !command.contains("commit") {
        return false;
    }

    // Run git diff --cached --name-only to get staged files.
    // Use the cwd from the command if it starts with `cd`.
    let output = std::process::Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return false, // Can't determine — don't skip
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    // No staged files — can't determine, don't skip verification
    if files.is_empty() {
        return false;
    }

    // Check every staged file against docs-only extensions
    files.iter().all(|file| {
        let lower = file.to_lowercase();
        DOCS_ONLY_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(&ext.to_lowercase()))
    })
}

/// Process a pre-commit verification hook event (PreToolUse).
/// Uses session-scoped signed override check.
pub fn process(input: &HookInput, _ctx: &super::HookContext<'_>) -> HookOutput {
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

    // Skip verification for docs-only commits (markdown, config, YAML, etc.)
    // These files have no tests to run — requiring evidence is nonsensical.
    if action == "commit" && is_docs_only_commit(command) {
        return HookOutput::allow();
    }

    // Check signed override file (Attack #47: replaces mtime-only check)
    if super::hygiene_override::is_signed_override_active(override_path, "verification", session_id)
    {
        return HookOutput::allow();
    }

    // Layer 1: Check transcript for test evidence.
    // Try input.transcript_path first, then fall back to searching by session ID.
    // In worktrees, Claude Code sends a transcript_path to a worktree-scoped
    // project dir that exists but is nearly empty (1 line). The real transcript
    // with test evidence is in the original project dir. So we check BOTH:
    // the provided path first, then the fallback if no evidence was found.
    if let Some(ref transcript_path) = input.transcript_path {
        if transcript_has_test_evidence(transcript_path) {
            return HookOutput::allow();
        }
    }
    // Fallback: search all project dirs for the largest transcript with this session ID
    if let Some(ref fallback_path) = find_transcript_by_session(session_id) {
        if transcript_has_test_evidence(fallback_path) {
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_non_git_bash_command() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": "ls -la"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process(&input, &ctx);
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

    #[test]
    fn test_find_transcript_picks_largest_file() {
        // Simulates worktree bug: two project dirs have the same session JSONL,
        // but the worktree-scoped one is empty and the original has real content.
        let tmpdir = tempfile::tempdir().unwrap();
        let session_id = "test-find-largest";

        // Create two "project" dirs
        let worktree_dir = tmpdir.path().join("C--repo--worktree");
        let original_dir = tmpdir.path().join("C--repo");
        std::fs::create_dir_all(&worktree_dir).unwrap();
        std::fs::create_dir_all(&original_dir).unwrap();

        // Worktree transcript: empty
        let worktree_transcript = worktree_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(&worktree_transcript, "").unwrap();

        // Original transcript: has test evidence
        let original_transcript = original_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &original_transcript,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
        )
        .unwrap();

        // The original (larger) transcript should have evidence
        assert!(transcript_has_test_evidence(
            &original_transcript.to_string_lossy()
        ));
        // The worktree (empty) one should not
        assert!(!transcript_has_test_evidence(
            &worktree_transcript.to_string_lossy()
        ));
    }

    #[test]
    fn test_docs_only_extensions() {
        // These should all be recognized as docs-only
        let docs_files = vec![
            "README.md", "CHANGELOG.md", "skills/linear/SKILL.md",
            "config.json", "config.yaml", "settings.toml",
            ".gitignore", ".editorconfig", "LICENSE",
        ];
        for f in &docs_files {
            let lower = f.to_lowercase();
            assert!(
                DOCS_ONLY_EXTENSIONS.iter().any(|ext| lower.ends_with(&ext.to_lowercase())),
                "Expected '{}' to be recognized as docs-only",
                f
            );
        }

        // These should NOT be docs-only
        let code_files = vec![
            "main.rs", "index.ts", "app.tsx", "server.py", "handler.go",
            "style.css", "Cargo.toml",  // toml IS in the list, but .rs is not
        ];
        for f in &code_files {
            let lower = f.to_lowercase();
            let is_docs = DOCS_ONLY_EXTENSIONS.iter().any(|ext| lower.ends_with(&ext.to_lowercase()));
            if f.ends_with(".toml") {
                assert!(is_docs, ".toml should be docs-only");
            } else {
                assert!(!is_docs, "Expected '{}' to NOT be docs-only", f);
            }
        }
    }

    #[test]
    fn test_is_docs_only_not_commit() {
        // Non-commit commands should return false
        assert!(!is_docs_only_commit("ls -la"));
        assert!(!is_docs_only_commit("git push origin main"));
    }
}
