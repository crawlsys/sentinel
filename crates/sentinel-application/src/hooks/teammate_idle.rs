//! `TeammateIdle` hook — quality gate for agent team members going idle
//!
//! When a teammate is about to go idle, checks if they have uncompleted tasks
//! and reminds them to check the task list before going idle.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process `TeammateIdle` event
///
/// Injects context reminding the teammate to check for remaining work.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    // SEN-1: drop malformed TeammateIdle events. If the dispatcher didn't
    // populate teammate_name with a real value, the event is malformed —
    // emitting "Teammate 'unknown' is going idle" just spams the lead session.
    let teammate_name = match input.extra.get("teammate_name").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() && s != "unknown" => s,
        _ => return HookOutput::allow(),
    };

    // NOTE: the CC hook payload's `team_name` field is @deprecated as of
    // 2.1.201 ("Sessions have a single implicit team… will be removed in a
    // future release"), so we no longer read it. It only ever fed decorative
    // label strings and a write-only channel-event meta stamp that no consumer
    // reads back; the idle-debounce key is (session_id, teammate) and is
    // unaffected. The contract-diff guard (cc-boundary-contract.tsv) still
    // watches for CC actually removing the field.

    // Inject a reminder to check task list before going idle
    let context = format!(
        "[Team Quality Gate] Teammate '{teammate_name}' is going idle.\n\
         \n\
         Before going idle, ensure:\n\
         1. All assigned tasks are marked completed (TaskUpdate with status: completed)\n\
         2. Any blockers or issues are reported to the team lead via SendMessage\n\
         3. Check TaskList for any unblocked tasks you can claim\n\
         4. If no more work available, acknowledge to the lead before going idle"
    );

    // Emit channel event so the lead session gets a real-time push
    // notification — debounced per (session, teammate). Claude Code fires
    // TeammateIdle on every idle poll, so one idle agent produces the same
    // event several times a minute; one push per cooldown window is signal,
    // the rest is spam. The context injection below is NOT debounced — the
    // quality-gate reminder is meant for every poll.
    if should_emit_idle_event(ctx.fs, input.session_id.as_deref(), teammate_name) {
        let summary = format!("Teammate '{teammate_name}' is going idle.");
        let mut meta = serde_json::Map::new();
        meta.insert(
            "teammate_name".to_string(),
            serde_json::Value::String(teammate_name.to_string()),
        );
        crate::channel_events::emit(
            ctx.fs,
            ctx.env,
            "teammate_idle",
            &summary,
            meta,
            input.session_id.as_deref(),
            input.cwd.as_deref(),
            Some(teammate_name),
        );
    }

    HookOutput::inject_context(HookEvent::TeammateIdle, &context)
}

/// Minimum gap between two emitted idle events for the same
/// (session, teammate) pair.
const IDLE_EMIT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Marker whose mtime records the last emitted idle event for this
/// (session, teammate) pair.
fn idle_debounce_marker(
    fs: &dyn super::FileSystemPort,
    session_id: &str,
    teammate: &str,
) -> std::path::PathBuf {
    let safe: String = teammate
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    fs.claude_dir()
        .join("sentinel")
        .join("state")
        .join("idle-debounce")
        .join(format!("{session_id}_{safe}"))
}

/// True when no idle event was emitted for this pair within the cooldown
/// window; refreshes the marker when emission is due. Sessions without a
/// concrete id are never debounced (`emit` drops those events anyway).
fn should_emit_idle_event(
    fs: &dyn super::FileSystemPort,
    session_id: Option<&str>,
    teammate: &str,
) -> bool {
    let Some(session_id) = session_id.and_then(super::session_path_component) else {
        return true;
    };
    let marker = idle_debounce_marker(fs, session_id, teammate);
    let recently = fs
        .metadata(&marker)
        .and_then(|m| m.modified().map_err(Into::into))
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < IDLE_EMIT_COOLDOWN);
    if recently {
        return false;
    }
    if let Some(parent) = marker.parent() {
        let _ = fs.create_dir_all(parent);
    }
    let _ = fs.write(&marker, b"1");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_teammate_idle_injects_context() {
        let mut input = HookInput::default();
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        // team_name is intentionally NOT set — the field is @deprecated in CC
        // and no longer read; a stray value must not surface in the context.
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("my-project"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("backend-dev"));
        assert!(ctx.contains("TaskList"));
        // The deprecated team_name must NOT leak into the injected context,
        // even when the upstream still sends it.
        assert!(
            !ctx.contains("my-project"),
            "deprecated team_name must not appear in the idle context"
        );
        assert!(
            !ctx.contains("team:"),
            "the (team: …) clause must be gone"
        );
    }

    #[test]
    fn test_teammate_idle_drops_event_when_teammate_name_missing() {
        // SEN-1: a TeammateIdle event without a real teammate_name is
        // malformed and must be dropped.
        let input = HookInput::default();
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_teammate_idle_drops_event_when_teammate_name_is_unknown_literal() {
        // SEN-1: also drop events where the upstream populated the literal
        // string "unknown".
        let mut input = HookInput::default();
        input
            .extra
            .insert("teammate_name".to_string(), serde_json::json!("unknown"));
        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn idle_channel_event_is_debounced_within_cooldown() {
        use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        let mut input = HookInput {
            session_id: Some("idle-sess-1".to_string()),
            ..Default::default()
        };
        input.extra.insert(
            "teammate_name".to_string(),
            serde_json::json!("backend-dev"),
        );
        input
            .extra
            .insert("team_name".to_string(), serde_json::json!("my-team"));

        // Claude Code fires TeammateIdle on every idle poll — three polls
        // inside the cooldown window must produce exactly ONE channel event.
        process(&input, &ctx);
        process(&input, &ctx);
        let output = process(&input, &ctx);

        let pending = crate::channel_events::pending_events_for_session(&fs, Some("idle-sess-1"));
        assert_eq!(pending.len(), 1, "idle emits must be debounced");

        // The quality-gate context injection itself is NOT debounced.
        assert!(output.hook_specific_output.is_some());
    }

    #[test]
    fn idle_events_for_different_teammates_are_not_cross_debounced() {
        use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);

        for teammate in ["alpha-dev", "beta-dev"] {
            let mut input = HookInput {
                session_id: Some("idle-sess-2".to_string()),
                ..Default::default()
            };
            input
                .extra
                .insert("teammate_name".to_string(), serde_json::json!(teammate));
            process(&input, &ctx);
        }

        let pending = crate::channel_events::pending_events_for_session(&fs, Some("idle-sess-2"));
        assert_eq!(
            pending.len(),
            2,
            "each teammate gets its own debounce window"
        );
    }
}
