//! Stdin Parser
//!
//! Parses JSON from stdin with Windows backslash resilience.
//! Handles the double-backslash encoding Claude Code uses on Windows.

use anyhow::{Context, Result};

use sentinel_domain::events::HookInput;

/// Read and parse hook input from stdin
pub fn read_hook_input() -> Result<HookInput> {
    let mut buffer = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer)
        .context("Failed to read stdin")?;

    parse_hook_input(&buffer)
}

/// Parse hook input from a string (testable without stdin)
pub fn parse_hook_input(raw: &str) -> Result<HookInput> {
    // Try direct parse first
    if let Ok(input) = serde_json::from_str::<HookInput>(raw) {
        return Ok(input);
    }

    // Windows backslash fix: Claude Code sometimes double-encodes paths
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
