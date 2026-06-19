//! `SessionEnd` hook — cleanup on session termination
//!
//! Called when the Claude Code session ends. Has a very tight timeout
//! (1.5s default via `CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS`).
//! Must be extremely fast — only essential cleanup.

use sentinel_domain::events::{HookInput, HookOutput};

use super::concrete_input_session_id;

/// Process `SessionEnd` event
///
/// Performs minimal cleanup: flush pending state to disk.
/// Must complete within 1.5s — no network calls, no heavy I/O.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let reason = input
        .extra
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let Some(session_id) = concrete_input_session_id(input) else {
        tracing::info!(reason, "Session ending without concrete session id");
        return HookOutput::allow();
    };

    tracing::info!(session_id, reason, "Session ending");

    // Flush any buffered telemetry/metrics
    if let Some(home) = ctx.fs.home_dir() {
        let metrics_dir = super::metrics_dir(&home);
        let end_entry = serde_json::json!({
            "event": "session_end",
            "session_id": session_id,
            "reason": reason,
            "ts": chrono::Utc::now().to_rfc3339(),
        });

        let _ = ctx.fs.create_dir_all(&metrics_dir);
        let line = format!("{end_entry}\n");
        let _ = ctx
            .fs
            .append(&metrics_dir.join("sessions.jsonl"), line.as_bytes());
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    #[test]
    fn test_session_end_allows() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("prompt_input_exit"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn missing_session_does_not_append_unknown_metrics_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let metrics_file = crate::hooks::metrics_dir(tmp.path()).join("sessions.jsonl");

        let mut input = HookInput::default();
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("prompt_input_exit"));
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(
            !metrics_file.exists(),
            "missing session must not append a durable unknown session row"
        );
    }

    #[test]
    fn default_session_does_not_append_metrics_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let metrics_file = crate::hooks::metrics_dir(tmp.path()).join("sessions.jsonl");

        let input = HookInput {
            session_id: Some("default".to_string()),
            ..Default::default()
        };
        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(
            !metrics_file.exists(),
            "synthetic default session must not append metrics"
        );
    }

    #[test]
    fn concrete_session_appends_metrics_row() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let metrics_file = crate::hooks::metrics_dir(tmp.path()).join("sessions.jsonl");

        let mut input = HookInput {
            session_id: Some("session-end-real".to_string()),
            ..Default::default()
        };
        input
            .extra
            .insert("reason".to_string(), serde_json::json!("prompt_input_exit"));
        let output = process(&input, &ctx);
        let line = std::fs::read_to_string(&metrics_file)
            .expect("session end metrics")
            .lines()
            .next()
            .expect("metrics row")
            .to_string();
        let entry: serde_json::Value = serde_json::from_str(&line).unwrap();

        assert!(output.blocked.is_none());
        assert_eq!(entry["session_id"], "session-end-real");
        assert_eq!(entry["reason"], "prompt_input_exit");
    }
}
