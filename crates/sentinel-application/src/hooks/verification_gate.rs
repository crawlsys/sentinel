//! Verification Gate
//!
//! Detects unverified completion claims in Claude's transcript and warns
//! via stderr. Inspired by the "evidence before claims" pattern.
//!
//! Logic:
//! 1. Read transcript for completion claims (regex fast path)
//! 2. If no claims -> exit (no-op)
//! 3. If claims -> scan for verification evidence (tool calls, test output)
//! 4. If evidence found -> log success
//! 5. If NO evidence -> warn via stderr
//! 6. Cooldown: max 1 warning per 5 minutes

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cooldown duration in milliseconds (5 minutes).
const COOLDOWN_MS: u128 = 5 * 60 * 1000;

/// Get the cooldown file path for a session.
fn cooldown_path(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-verification-cooldown-{session_id}"))
}

/// Get the offset file path for a session.
fn offset_path(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-verification-offset-{session_id}"))
}

/// Check if we're still within the cooldown period.
fn is_on_cooldown(session_id: &str) -> bool {
    let path = cooldown_path(session_id);
    let last_warn: u128 = fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);

    if last_warn == 0 {
        return false;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    now.saturating_sub(last_warn) < COOLDOWN_MS
}

/// Set the cooldown marker to now.
fn set_cooldown(session_id: &str) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let _ = fs::write(cooldown_path(session_id), now.to_string());
}

/// Read transcript offset for dedup.
fn read_offset(session_id: &str) -> usize {
    fs::read_to_string(offset_path(session_id))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Write transcript offset.
fn write_offset(session_id: &str, offset: usize) {
    let _ = fs::write(offset_path(session_id), offset.to_string());
}

/// Completion claim patterns.
fn completion_patterns() -> Vec<Regex> {
    [
        r"(?i)all tests pass",
        r"(?i)tests are passing",
        r"(?i)implementation (?:is )?complete",
        r"(?i)ready to merge",
        r"(?i)feature is done",
        r"(?i)everything works",
        r"(?i)bug is fixed",
        r"(?i)all (?:checks|tests) (?:are )?green",
        r"(?i)successfully implemented",
        r"(?i)changes are working",
        r"(?i)build succeeds",
        r"(?i)all.*passing",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
}

/// Evidence patterns that validate completion claims.
fn evidence_patterns() -> Vec<Regex> {
    [
        r"(?i)\d+\s+pass(?:ing|ed)?",
        r"(?i)\d+\s+fail(?:ing|ed)?",
        r"(?i)exit code:?\s*0",
        r"(?i)tests?\s+suites?.*passed",
        r"[\u2713\u2714]|PASS",
        r"(?i)BUILD SUCCESS",
        r"(?i)Successfully compiled",
        r"tsc.*--noEmit",
        r"(?i)npm test|npx vitest|jest|cargo test",
        r"(?i)\$ .*(test|build|lint|check)",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
}

/// Parse transcript and extract assistant text + tool call info.
struct TranscriptData {
    assistant_text: String,
    has_bash_calls: bool,
}

fn parse_transcript(transcript_path: &str, session_id: &str) -> TranscriptData {
    let mut data = TranscriptData {
        assistant_text: String::new(),
        has_bash_calls: false,
    };

    let content = match fs::read_to_string(transcript_path) {
        Ok(c) => c,
        Err(_) => return data,
    };

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let offset = read_offset(session_id).min(total);

    for line in &lines[offset..] {
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(content) = entry
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                data.assistant_text.push_str(text);
                                data.assistant_text.push('\n');
                            }
                        }
                        "tool_use" => {
                            if block.get("name").and_then(|v| v.as_str()) == Some("Bash") {
                                data.has_bash_calls = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // tool_result entries also count as evidence
        if entry.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
            data.has_bash_calls = true;
        }
    }

    write_offset(session_id, total);
    data
}

/// Process the verification-gate hook event (Stop).
pub fn process(input: &HookInput) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    let transcript_path = match &input.transcript_path {
        Some(p) if !p.is_empty() => p.clone(),
        _ => return HookOutput::allow(),
    };

    let data = parse_transcript(&transcript_path, session_id);

    if data.assistant_text.trim().is_empty() {
        return HookOutput::allow();
    }

    // Scan for completion claims
    let claim_patterns = completion_patterns();
    let mut claims_found: Vec<String> = Vec::new();
    for pattern in &claim_patterns {
        if let Some(mat) = pattern.find(&data.assistant_text) {
            claims_found.push(mat.as_str().to_string());
        }
    }

    if claims_found.is_empty() {
        return HookOutput::allow();
    }

    // Scan for evidence
    let ev_patterns = evidence_patterns();
    let mut evidence_found = data.has_bash_calls;

    if !evidence_found {
        for pattern in &ev_patterns {
            if pattern.is_match(&data.assistant_text) {
                evidence_found = true;
                break;
            }
        }
    }

    if evidence_found {
        tracing::debug!(
            claims = claims_found.len(),
            "verification gate: claims verified with evidence"
        );
        return HookOutput::allow();
    }

    // Claims without evidence -- warn if not on cooldown
    if is_on_cooldown(session_id) {
        return HookOutput::allow();
    }

    set_cooldown(session_id);

    // Build warning box
    let mut warning_lines = vec![
        String::new(),
        "+-----------------------------------------------------------+".to_string(),
        "|  WARNING: Completion claimed without evidence              |".to_string(),
        "+-----------------------------------------------------------+".to_string(),
    ];

    for claim in claims_found.iter().take(3) {
        let truncated = if claim.len() > 50 {
            &claim[..50]
        } else {
            claim
        };
        warning_lines.push(format!(
            "|    -> \"{}\"{}|",
            truncated,
            " ".repeat(55 - truncated.len().min(55) - 6)
        ));
    }

    warning_lines.extend([
        "|                                                           |".to_string(),
        "|  Run tests, build, or other verification commands         |".to_string(),
        "|  before claiming completion.                              |".to_string(),
        "+-----------------------------------------------------------+".to_string(),
        String::new(),
    ]);

    let warning = warning_lines.join("\n");
    tracing::warn!("{}", warning);

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;

    fn make_transcript(entries: &[serde_json::Value]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for entry in entries {
            writeln!(file, "{}", serde_json::to_string(entry).unwrap()).unwrap();
        }
        file
    }

    #[test]
    fn test_no_transcript() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_no_claims_returns_allow() {
        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "I've made some progress on the feature."
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some("test-vg-noclaims".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_claim_with_evidence_passes() {
        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "text",
                        "text": "All tests pass. 42 passing, 0 failing."
                    },
                    {
                        "type": "tool_use",
                        "name": "Bash",
                        "input": { "command": "npm test" }
                    }
                ]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some("test-vg-evidence".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_claim_without_evidence_warns() {
        // Use a unique session ID so cooldown doesn't interfere
        let session_id = format!("test-vg-warn-{}", std::process::id());

        // Clear any existing cooldown
        let _ = fs::remove_file(cooldown_path(&session_id));
        let _ = fs::remove_file(offset_path(&session_id));

        let transcript = make_transcript(&[json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "All tests pass and the implementation is complete."
                }]
            }
        })]);

        let input = HookInput {
            transcript_path: Some(transcript.path().to_string_lossy().to_string()),
            session_id: Some(session_id.clone()),
            ..Default::default()
        };
        let output = process(&input);
        // Should still allow (Stop hooks never block) but should have warned
        assert!(output.blocked.is_none());

        // Clean up
        let _ = fs::remove_file(cooldown_path(&session_id));
        let _ = fs::remove_file(offset_path(&session_id));
    }

    #[test]
    fn test_cooldown_prevents_repeated_warnings() {
        let session_id = format!("test-vg-cooldown-{}", std::process::id());

        // Set cooldown to now
        set_cooldown(&session_id);
        assert!(is_on_cooldown(&session_id));

        // Clean up
        let _ = fs::remove_file(cooldown_path(&session_id));
    }

    #[test]
    fn test_expired_cooldown() {
        let session_id = format!("test-vg-expired-{}", std::process::id());

        // Write a cooldown timestamp from 10 minutes ago
        let ten_min_ago = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis().saturating_sub(10 * 60 * 1000))
            .unwrap_or(0);
        let _ = fs::write(cooldown_path(&session_id), ten_min_ago.to_string());

        assert!(!is_on_cooldown(&session_id));

        // Clean up
        let _ = fs::remove_file(cooldown_path(&session_id));
    }

    #[test]
    fn test_completion_patterns_match() {
        let patterns = completion_patterns();
        let test_cases = [
            "all tests pass",
            "The implementation is complete",
            "Ready to merge now",
            "The bug is fixed",
        ];
        for text in &test_cases {
            let matched = patterns.iter().any(|p| p.is_match(text));
            assert!(matched, "Expected pattern match for: {text}");
        }
    }

    #[test]
    fn test_evidence_patterns_match() {
        let patterns = evidence_patterns();
        let test_cases = [
            "42 passing",
            "exit code: 0",
            "BUILD SUCCESS",
            "$ npm test",
        ];
        for text in &test_cases {
            let matched = patterns.iter().any(|p| p.is_match(text));
            assert!(matched, "Expected evidence match for: {text}");
        }
    }
}
