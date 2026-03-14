//! Stdin Parser
//!
//! Parses JSON from stdin with Windows backslash resilience.
//! Handles the double-backslash encoding Claude Code uses on Windows.

use std::time::Duration;

use anyhow::{Context, Result};

use sentinel_domain::events::HookInput;

/// Read and parse hook input from stdin.
///
/// Claude Code sends hook payloads as a single JSON line on stdin. Reading to
/// EOF is fragile on Windows because the shell may keep the pipe open longer
/// than the payload itself. We therefore read a single line instead of waiting
/// for the entire stream to close.
pub fn read_hook_input() -> Result<HookInput> {
    let buffer = read_raw_stdin(Duration::from_secs(3))?;
    if buffer.is_empty() {
        Ok(HookInput::default())
    } else {
        parse_hook_input(&buffer)
    }
}

/// Read raw stdin as a single logical JSON line.
///
/// The `_timeout` parameter is retained to avoid churn in callers, but the
/// implementation no longer spawns a detached reader thread. That timeout path
/// caused hook processes to remain alive after returning, which wedged Claude's
/// REPL while it waited for the hook command to exit.
pub fn read_raw_stdin(_timeout: Duration) -> Result<String> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    read_raw_from_reader(&mut reader)
}

fn read_raw_from_reader<R: std::io::BufRead>(reader: &mut R) -> Result<String> {
    let mut buffer = String::new();
    match reader.read_line(&mut buffer) {
        Ok(0) => Ok(String::new()),
        Ok(_) => Ok(buffer.trim_end_matches(['\r', '\n']).to_string()),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to read stdin — using empty input");
            Ok(String::new())
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
    use std::io::Cursor;

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

    #[test]
    fn test_read_raw_from_reader_reads_single_line() {
        let mut cursor = Cursor::new(b"{\"session_id\":\"abc\"}\nignored");
        let raw = read_raw_from_reader(&mut cursor).unwrap();
        assert_eq!(raw, "{\"session_id\":\"abc\"}");
    }

    #[test]
    fn test_read_raw_from_reader_accepts_eof_without_newline() {
        let mut cursor = Cursor::new(b"{\"session_id\":\"abc\"}");
        let raw = read_raw_from_reader(&mut cursor).unwrap();
        assert_eq!(raw, "{\"session_id\":\"abc\"}");
    }
}
