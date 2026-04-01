//! StopFailure hook — detect API errors and rate limits
//!
//! Called when a turn ends due to an API error rather than normal completion.
//! Can detect rate limits, overloaded errors, etc.

use sentinel_domain::events::{HookInput, HookOutput};

/// Process StopFailure event
///
/// Logs the error for diagnostics. Could trigger notifications for
/// persistent failures.
pub fn process(input: &HookInput) -> HookOutput {
    let error = input
        .extra
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let error_details = input
        .extra
        .get("error_details")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    tracing::warn!(error, error_details, "Turn ended with API error");

    // Log to errors.jsonl for diagnostics
    if let Some(home) = dirs::home_dir() {
        let metrics_dir = home.join(".claude").join("metrics");
        let entry = serde_json::json!({
            "event": "stop_failure",
            "error": error,
            "error_details": error_details,
            "session_id": input.session_id,
            "ts": chrono::Utc::now().to_rfc3339(),
        });

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(metrics_dir.join("errors.jsonl"))
        {
            use std::io::Write;
            let _ = writeln!(file, "{}", entry);
        }
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stop_failure_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("error".to_string(), serde_json::json!("rate_limit"));

        let output = process(&input);
        assert!(output.blocked.is_none());
    }
}
