//! Sentinel-recorded test/build evidence
//!
//! Pure types and pattern-matching for the evidence-recording system that
//! replaces transcript-parsing in `pre_commit_verification`.
//!
//! ## Why this exists
//!
//! Hook input `session_id` is the harness wrapper ID, which does **not**
//! match the on-disk `{sessionId}.jsonl` filename Claude Code writes
//! (an internal `G8.sessionId`). Searching the transcript directory by the
//! hook's session_id never finds the right file, so transcript-based
//! verification falsely blocks every commit/push.
//!
//! Sentinel sees the same `session_id` on both `PostToolUse` (when the
//! test ran) and `PreToolUse` (when the commit fires). Recording our own
//! evidence file keyed by that ID makes the lookup deterministic and
//! cuts the fragile dependency on Claude Code's transcript format.
//!
//! ## File layout
//!
//! `<home>/.claude/sentinel/state/test-evidence/<session_id>.jsonl`
//!
//! Append-only JSONL. One line per recorded test/build invocation.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Test/build command patterns that count as verification evidence.
///
/// Source of truth for both the recorder (`test_evidence_recorder` hook,
/// PostToolUse) and the reader (`pre_commit_verification`, PreToolUse).
/// Keeping the list in the domain layer ensures both sides agree on what
/// "running tests" means.
pub const TEST_COMMAND_PATTERNS: &[&str] = &[
    r"\bnpm\s+test\b",
    r"\bnpx\s+(vitest|jest|mocha|cypress)\b",
    r"\byarn\s+test\b",
    r"\bpnpm\s+test\b",
    r"\bcargo\s+test\b",
    r"\bcargo\s+build\b",
    r"\bcargo\s+check\b",
    r"\bcargo\s+clippy\b",
    r"\bpytest\b",
    r"\bgo\s+test\b",
    r"\bgo\s+build\b",
    r"\bgo\s+vet\b",
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

/// One recorded test/build invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestEvidenceEntry {
    /// Unix epoch milliseconds when the entry was recorded.
    pub ts_ms: u128,

    /// Session ID as observed by the recording hook. Same value the
    /// pre-commit reader sees, so lookup is exact.
    pub session_id: String,

    /// Working directory at the time the command was issued.
    pub cwd: String,

    /// The full Bash command string the agent invoked.
    pub command: String,

    /// Whether the tool reported success. The reader treats this as
    /// advisory — for parity with the legacy transcript-based behavior
    /// (which only checked "did a test command appear?"), the reader
    /// accepts the entry regardless of `success`. Stored anyway so we
    /// can tighten the policy later without re-recording.
    pub success: bool,
}

/// Path to the sentinel-owned evidence file for a given session.
#[must_use]
pub fn evidence_path(home: &Path, session_id: &str) -> PathBuf {
    home.join(".claude")
        .join("sentinel")
        .join("state")
        .join("test-evidence")
        .join(format!("{session_id}.jsonl"))
}

/// Compile the [`TEST_COMMAND_PATTERNS`] list once.
///
/// Lives in the domain so callers (hook recorder + hook reader) build
/// from the same regex source. Filters out invalid patterns silently —
/// the patterns are static, so a malformed entry would already fail in
/// CI long before reaching production.
#[must_use]
pub fn compile_command_patterns() -> Vec<regex::Regex> {
    TEST_COMMAND_PATTERNS
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect()
}

/// Does `command` match any test/build pattern?
///
/// Convenience wrapper that compiles patterns lazily for one-shot
/// checks. Hot paths (the recorder) should compile once and reuse.
#[must_use]
pub fn command_matches_test_pattern(command: &str) -> bool {
    compile_command_patterns()
        .iter()
        .any(|r| r.is_match(command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn recognises_common_test_commands() {
        for cmd in [
            "npm test",
            "cargo test",
            "cargo build --release",
            "pnpm test --watch",
            "pytest -v",
            "go test ./...",
            "npx vitest run",
            "tsc --noEmit",
        ] {
            assert!(
                command_matches_test_pattern(cmd),
                "expected pattern match for: {cmd}"
            );
        }
    }

    #[test]
    fn ignores_non_test_commands() {
        for cmd in ["ls -la", "git commit -m foo", "rm -rf node_modules"] {
            assert!(
                !command_matches_test_pattern(cmd),
                "did not expect pattern match for: {cmd}"
            );
        }
    }

    #[test]
    fn evidence_path_is_under_sentinel_state() {
        let home = PathBuf::from("/home/user");
        let path = evidence_path(&home, "abc-123");
        assert!(
            path.ends_with(".claude/sentinel/state/test-evidence/abc-123.jsonl")
                || path.ends_with(r".claude\sentinel\state\test-evidence\abc-123.jsonl"),
            "got: {}",
            path.display()
        );
    }

    #[test]
    fn entry_round_trips_through_jsonl() {
        let entry = TestEvidenceEntry {
            ts_ms: 1_700_000_000_000,
            session_id: "sess-xyz".into(),
            cwd: "/repo".into(),
            command: "cargo test".into(),
            success: true,
        };
        let line = serde_json::to_string(&entry).unwrap();
        let parsed: TestEvidenceEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, entry);
    }
}
