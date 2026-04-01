//! SessionEnd hook — cleanup on session termination
//!
//! Called when the Claude Code session ends. Has a very tight timeout
//! (1.5s default via CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS).
//! Must be extremely fast — only essential cleanup.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process SessionEnd event
///
/// Performs minimal cleanup: flush pending state to disk.
/// Must complete within 1.5s — no network calls, no heavy I/O.
pub fn process(input: &HookInput) -> HookOutput {
    let reason = input
        .extra
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let session_id = input.session_id.as_deref().unwrap_or("unknown");

    tracing::info!(session_id, reason, "Session ending");

    // Flush any buffered telemetry/metrics
    if let Some(home) = dirs::home_dir() {
        let metrics_dir = home.join(".claude").join("metrics");
        let end_entry = serde_json::json!({
            "event": "session_end",
            "session_id": session_id,
            "reason": reason,
            "ts": chrono::Utc::now().to_rfc3339(),
        });

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(metrics_dir.join("sessions.jsonl"))
        {
            use std::io::Write;
            let _ = writeln!(file, "{}", end_entry);
        }
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_end_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("prompt_input_exit"));

        let output = process(&input);
        assert!(output.blocked.is_none());
    }
}
