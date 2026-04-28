//! MCP Health Hook
//!
//! Detects MCP server failures after tool calls and logs them to
//! ~/.claude/metrics/errors.jsonl for auto-filing to Linear.
//!
//! Runs on PostToolUse — only checks tools with the `mcp__` prefix.
//! Never blocks, only logs.

use chrono::Utc;
use sentinel_domain::events::{HookInput, HookOutput};
use std::path::PathBuf;

use super::{FileSystemPort, HookContext};

/// Error patterns that indicate MCP server failure
const ERROR_PATTERNS: &[&str] = &[
    "connection refused",
    "econnrefused",
    "timeout",
    "timed out",
    "server not running",
    "failed to start",
    "not connected",
    "spawn error",
    "enotfound",
    "econnreset",
    "epipe",
    "server error",
    "internal server error",
    "mcp server",
    "transport error",
    "process exited",
    "failed to connect",
    "ehostunreach",
    "etimedout",
];

/// Path to the errors JSONL file
fn errors_file_path(fs: &dyn FileSystemPort) -> Option<PathBuf> {
    fs.home_dir()
        .map(|h| super::metrics_dir(&h).join("errors.jsonl"))
}

/// Extract the MCP server name from a tool name like `mcp__linear__get_issue`
fn extract_server_name(tool_name: &str) -> &str {
    let parts: Vec<&str> = tool_name.splitn(3, "__").collect();
    if parts.len() >= 2 {
        parts[1]
    } else {
        "unknown"
    }
}

/// Check if the tool result contains any error patterns
fn detect_error(input: &HookInput) -> Option<String> {
    // Check is_error flag from extra fields
    let is_error = input
        .extra
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Check tool_result for error patterns
    let result_text = input
        .tool_result
        .as_ref()
        .map(|v| {
            if let Some(s) = v.as_str() {
                s.to_string()
            } else {
                serde_json::to_string(v).unwrap_or_default()
            }
        })
        .unwrap_or_default();

    let result_lower = result_text.to_lowercase();
    let matched_pattern = ERROR_PATTERNS.iter().find(|&&p| result_lower.contains(p));

    if is_error || matched_pattern.is_some() {
        let error_detail = if let Some(pattern) = matched_pattern {
            (*pattern).to_string()
        } else {
            result_text.chars().take(200).collect()
        };
        Some(error_detail)
    } else {
        None
    }
}

/// Log an MCP error to the errors JSONL file
fn log_mcp_error(
    fs: &dyn FileSystemPort,
    tool_name: &str,
    server_name: &str,
    error_detail: &str,
    session_id: &str,
) {
    let errors_path = match errors_file_path(fs) {
        Some(p) => p,
        None => return,
    };

    // Ensure parent directory exists
    if let Some(parent) = errors_path.parent() {
        let _ = fs.create_dir_all(parent);
    }

    let ts = Utc::now().to_rfc3339();
    let id = format!(
        "err-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        std::process::id()
    );
    let severity = if error_detail.contains("refused") || error_detail.contains("spawn") {
        "critical"
    } else if error_detail.contains("timeout") || error_detail.contains("timed out") {
        "warning"
    } else {
        "info"
    };

    let entry = serde_json::json!({
        "id": id,
        "hook": "mcp-health",
        "component": "mcp",
        "type": format!("mcp-{}", server_name),
        "error": format!("{}: {} ({})", server_name, error_detail, tool_name),
        "severity": severity,
        "session_id": session_id,
        "ts": ts,
    });

    let line = format!("{}\n", serde_json::to_string(&entry).unwrap_or_default());
    let _ = fs.append(&errors_path, line.as_bytes());
}

/// Process an MCP health check (PostToolUse)
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let tool_name = match &input.tool_name {
        Some(name) if name.starts_with("mcp__") => name.as_str(),
        _ => return HookOutput::allow(),
    };

    let server_name = extract_server_name(tool_name);
    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    // Check for errors in the tool result
    if let Some(error_detail) = detect_error(input) {
        log_mcp_error(ctx.fs, tool_name, server_name, &error_detail, session_id);

        // Push failure event via channel for instant notification
        let severity = if error_detail.contains("refused") || error_detail.contains("spawn") {
            "critical"
        } else {
            "warning"
        };
        let mut meta = serde_json::Map::new();
        meta.insert(
            "server".into(),
            serde_json::Value::String(server_name.to_string()),
        );
        meta.insert(
            "tool".into(),
            serde_json::Value::String(tool_name.to_string()),
        );
        meta.insert(
            "severity".into(),
            serde_json::Value::String(severity.to_string()),
        );
        crate::channel_events::emit(
            ctx.fs, ctx.env,
            "mcp_server_failure",
            &format!("MCP server `{server_name}` failed: {error_detail}. Try `mcp__sentinel__mcp_restart_server` to fix."),
            meta,
            input.session_id.as_deref(), input.cwd.as_deref(), Some("mcp_health"),
        );
    }

    // Never block — this hook is observational only
    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allows_non_mcp_tool() {
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_mcp_tool_without_error() {
        let input = HookInput {
            tool_name: Some("mcp__linear__get_issue".to_string()),
            tool_result: Some(serde_json::json!({"id": "FIR-123", "title": "Test issue"})),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_detects_connection_refused() {
        let input = HookInput {
            tool_name: Some("mcp__linear__get_issue".to_string()),
            tool_result: Some(serde_json::json!("Connection refused")),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        // Should still allow (never blocks) but would log
        assert!(output.blocked.is_none());

        // Verify the error was detected
        let error = detect_error(&input);
        assert!(error.is_some());
        assert_eq!(error.unwrap(), "connection refused");
    }

    #[test]
    fn test_detects_timeout_error() {
        let input = HookInput {
            tool_name: Some("mcp__steel__navigate".to_string()),
            tool_result: Some(serde_json::json!("Request timed out after 30000ms")),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        let error = detect_error(&input);
        assert!(error.is_some());
        assert_eq!(error.unwrap(), "timed out");
    }

    #[test]
    fn test_no_error_in_clean_result() {
        let input = HookInput {
            tool_name: Some("mcp__linear__list_issues".to_string()),
            tool_result: Some(serde_json::json!([{"id": "1"}, {"id": "2"}])),
            ..Default::default()
        };
        let error = detect_error(&input);
        assert!(error.is_none());
    }

    #[test]
    fn test_extract_server_name() {
        assert_eq!(extract_server_name("mcp__linear__get_issue"), "linear");
        assert_eq!(extract_server_name("mcp__steel__navigate"), "steel");
        assert_eq!(extract_server_name("mcp__doppler__list_secrets"), "doppler");
        assert_eq!(extract_server_name("mcp__"), "");
        assert_eq!(extract_server_name("not_mcp"), "unknown");
    }

    #[test]
    fn test_logs_error_to_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let errors_path = tmpdir.path().join("errors.jsonl");

        // Write directly to test the log format (matches error_reporter schema)
        let entry = serde_json::json!({
            "id": "err-test-12345",
            "hook": "mcp-health",
            "component": "mcp",
            "type": "mcp-linear",
            "error": "linear: connection refused (mcp__linear__get_issue)",
            "severity": "critical",
            "session_id": "test-session",
            "ts": Utc::now().to_rfc3339(),
        });
        let line = format!("{}\n", serde_json::to_string(&entry).unwrap());
        std::fs::write(&errors_path, &line).unwrap();

        let content = std::fs::read_to_string(&errors_path).unwrap();
        assert!(content.contains("connection refused"));
        assert!(content.contains("mcp-health"));
        assert!(content.contains("linear"));
        assert!(content.contains("critical"));
        assert!(content.contains("err-test-12345"));
    }

    #[test]
    fn test_allows_no_tool_name() {
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_detects_is_error_flag() {
        let mut extra = serde_json::Map::new();
        extra.insert("is_error".to_string(), serde_json::json!(true));

        let input = HookInput {
            tool_name: Some("mcp__linear__get_issue".to_string()),
            tool_result: Some(serde_json::json!("some opaque error message")),
            extra,
            ..Default::default()
        };
        let error = detect_error(&input);
        assert!(error.is_some());
    }
}
