//! Verification Gate — Two-phase hook
//!
//! **Stop phase:** Scans transcript for unverified completion claims
//! (e.g. "all tests pass" without running tests). Writes findings to
//! `~/.claude/metrics/unverified-claims.json`.
//!
//! **UserPromptSubmit phase:** Reads findings, checks cooldown (5 min),
//! injects reminder to run verification before claiming completion.

use regex::Regex;
use sentinel_domain::constants;
use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{FileSystemPort, HookContext};

/// Cooldown duration.
const COOLDOWN_MS: u64 = constants::HOOK_COOLDOWN_VERIFY_MS;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ClaimState {
    claims: Vec<String>,
    session_id: String,
    ts: String,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn state_file(fs: &dyn FileSystemPort, session_id: &str) -> Option<PathBuf> {
    let home = fs.home_dir()?;
    let dir = super::metrics_dir(&home);
    fs.create_dir_all(&dir).ok()?;
    Some(dir.join(format!("unverified-claims-{session_id}.json")))
}

/// Test-only override for the cooldown file path.
/// The STATE_LOCK mutex serializes all access, so a simple Mutex<Option> is safe.
#[cfg(test)]
static COOLDOWN_PATH_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn cooldown_file() -> PathBuf {
    #[cfg(test)]
    {
        if let Ok(guard) = COOLDOWN_PATH_OVERRIDE.lock() {
            if let Some(ref path) = *guard {
                return path.clone();
            }
        }
    }
    let session_id = std::env::var("CLAUDE_SESSION_ID")
        .or_else(|_| std::env::var("SESSION_ID"))
        .unwrap_or_else(|_| "default".to_string());
    std::env::temp_dir().join(format!("claude-verification-gate-{session_id}-last"))
}

fn cooldown_expired(fs: &dyn FileSystemPort) -> bool {
    let content = match fs.read_to_string(&cooldown_file()) {
        Ok(c) => c,
        Err(_) => return true,
    };
    let last: u64 = match content.trim().parse() {
        Ok(v) => v,
        Err(_) => return true,
    };
    now_ms().saturating_sub(last) >= COOLDOWN_MS
}

fn write_cooldown(fs_port: &dyn FileSystemPort) {
    let _ = fs_port.write(&cooldown_file(), now_ms().to_string().as_bytes());
}

/// Get the offset file path for a session.
fn offset_path(session_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("claude-verification-offset-{session_id}"))
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
        r"[\u{2713}\u{2714}]|PASS",
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

/// Parsed transcript data.
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

        if entry.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
            data.has_bash_calls = true;
        }
    }

    write_offset(session_id, total);
    data
}

// ---------------------------------------------------------------------------
// Stop phase: detect unverified claims and write state
// ---------------------------------------------------------------------------

pub fn process_stop(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
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
        // No claims — clear any previous state
        if let Some(path) = state_file(ctx.fs, session_id) {
            let _ = ctx.fs.write(&path, b"");
        }
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
        // Claims verified — clear state
        tracing::debug!(claims = claims_found.len(), "Claims verified with evidence");
        if let Some(path) = state_file(ctx.fs, session_id) {
            let _ = ctx.fs.write(&path, b"");
        }
        return HookOutput::allow();
    }

    // Unverified claims — write state
    let state = ClaimState {
        claims: claims_found.into_iter().take(5).collect(),
        session_id: session_id.to_string(),
        ts: chrono::Utc::now().to_rfc3339(),
    };

    if let Some(path) = state_file(ctx.fs, session_id) {
        let _ = ctx.fs.write(
            &path,
            serde_json::to_string(&state).unwrap_or_default().as_bytes(),
        );
    }

    tracing::warn!(
        count = state.claims.len(),
        "Unverified completion claims detected"
    );

    HookOutput::allow()
}

// ---------------------------------------------------------------------------
// UserPromptSubmit phase: inject verification reminder
// ---------------------------------------------------------------------------

pub fn process_prompt(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let path = match state_file(ctx.fs, session_id) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    let content = match ctx.fs.read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HookOutput::allow(),
    };

    let state: ClaimState = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return HookOutput::allow(),
    };

    // Only inject for the current session
    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    if state.session_id != session_id {
        return HookOutput::allow();
    }

    if !cooldown_expired(ctx.fs) {
        return HookOutput::allow();
    }

    write_cooldown(ctx.fs);

    let claims_list: String = state
        .claims
        .iter()
        .map(|c| format!("  - \"{c}\""))
        .collect::<Vec<_>>()
        .join("\n");

    let context = format!(
        "[Verification Gate] IRON LAW: No completion claims without fresh verification evidence.\n\
         \n\
         Your last response made these claims WITHOUT running verification:\n\
         {claims_list}\n\
         \n\
         BEFORE claiming completion, you MUST:\n\
         1. Run tests: `cargo test`, `npm test`, `vitest`, etc.\n\
         2. Check types: `tsc --noEmit`, `cargo check`\n\
         3. Build: `cargo build`, `npm run build`\n\
         4. See passing output in YOUR terminal (not from memory)\n\
         \n\
         COMMON RATIONALIZATIONS (all invalid):\n\
         - \"I already tested this earlier\" — Run it AGAIN. State may have changed.\n\
         - \"The change is too small to break anything\" — Small changes cause big bugs.\n\
         - \"I'm confident it works\" — Confidence is not evidence. Run the tests.\n\
         - \"The tests aren't relevant to my change\" — Let the tests prove that.\n\
         \n\
         RED FLAGS — If you catch yourself doing any of these, STOP:\n\
         - Claiming tests pass without a Bash() call showing output\n\
         - Saying \"should work\" or \"looks correct\" instead of showing proof\n\
         - Reporting completion on a task you haven't verified end-to-end\n\
         \n\
         The rule is simple: EVIDENCE FIRST, THEN CLAIMS. Never the reverse."
    );

    // Clear state after injecting (one-shot reminder)
    let _ = fs::remove_file(&path);

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;
    use std::sync::Mutex;

    /// Mutex to serialize tests that share the cooldown file.
    static STATE_LOCK: Mutex<()> = Mutex::new(());

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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_stop(&input, &ctx);
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_stop(&input, &ctx);
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
        let ctx = crate::hooks::test_support::stub_ctx(); let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_claim_without_evidence_writes_state() {
        let _lock = STATE_LOCK.lock().unwrap();
        let session_id = format!("test-vg-warn-{}", std::process::id());

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
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_stop(&input, &ctx);
        assert!(output.blocked.is_none());
        // StubFs writes are no-ops, so we just verify no panic
    }

    #[test]
    fn test_prompt_no_state_returns_allow() {
        // StubFs returns error on read → no state → allow
        let input = HookInput {
            session_id: Some("test-vg-inject".into()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process_prompt(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_cooldown_expired_with_stub() {
        let ctx = crate::hooks::test_support::stub_ctx();
        // StubFs returns error on read → expired
        assert!(cooldown_expired(ctx.fs));
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
        let test_cases = ["42 passing", "exit code: 0", "BUILD SUCCESS", "$ npm test"];
        for text in &test_cases {
            let matched = patterns.iter().any(|p| p.is_match(text));
            assert!(matched, "Expected evidence match for: {text}");
        }
    }
}
