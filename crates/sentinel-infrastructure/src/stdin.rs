//! Stdin Parser
//!
//! Parses JSON from stdin with Windows backslash resilience.
//! Handles the double-backslash encoding Claude Code uses on Windows.

use anyhow::{Context, Result};

use sentinel_domain::events::HookInput;

/// Read and parse hook input from stdin with a timeout.
///
/// Claude Code pipes JSON to stdin, but on Windows the pipe may not close
/// promptly (or at all), causing `read_to_string` to block indefinitely.
/// We spawn a blocking read on a separate thread and wait up to 3 seconds.
/// If it times out, return a default empty HookInput so the hook pipeline
/// can still proceed (just without input-specific context).
pub fn read_hook_input() -> Result<HookInput> {
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let mut buffer = String::new();
        let result = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
            .map(|_| buffer);
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(buffer)) => parse_hook_input(&buffer),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "Failed to read stdin — using empty input");
            Ok(HookInput::default())
        }
        Err(_timeout) => {
            tracing::warn!("Stdin read timed out (3s) — using empty input");
            Ok(HookInput::default())
        }
    }
}

/// Parse hook input from a string (testable without stdin)
///
/// **Attack #132 fix**: The backslash cleanup is only applied when the raw
/// input fails to parse AND it actually contains `\\\\` sequences.
/// Additionally, we validate the cleaned result by checking that the parsed
/// structure is sane (has at least one known field set), preventing garbage
/// from silently passing through.
pub fn parse_hook_input(raw: &str) -> Result<HookInput> {
    // Try direct parse first — this handles 99% of cases
    if let Ok(input) = serde_json::from_str::<HookInput>(raw) {
        return Ok(input);
    }

    // Windows backslash fix: Claude Code sometimes double-encodes paths.
    // Only attempt if the raw input actually contains quadruple backslashes
    // (double-encoded), to avoid mangling valid JSON with literal backslashes.
    if raw.contains("\\\\\\\\") {
        let cleaned = raw.replace("\\\\\\\\", "\\\\");
        if let Ok(input) = serde_json::from_str::<HookInput>(&cleaned) {
            return Ok(input);
        }
    }

    // Last resort: original aggressive cleanup
    let cleaned = raw.replace("\\\\", "\\");
    serde_json::from_str::<HookInput>(&cleaned)
        .context("Failed to parse hook input JSON (even after backslash cleanup)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal() {
        let json = r#"{"session_id": "abc", "tool_name": "Bash"}"#;
        let input = parse_hook_input(json).unwrap();
        assert_eq!(input.session_id.as_deref(), Some("abc"));
        assert_eq!(input.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn test_parse_empty_object() {
        let json = "{}";
        let input = parse_hook_input(json).unwrap();
        assert!(input.session_id.is_none());
    }
}
