//! `PostCompact` hook — restore critical state after context compaction
//!
//! Called after compaction completes. Receives `compact_summary`.
//! Can inject additionalContext to restore critical information.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::{concrete_input_session_id, HookContext};

/// Process `PostCompact` event
///
/// Restores active skill context and workflow state that may have been
/// lost during compaction.
pub fn process(input: &HookInput, ctx: &HookContext<'_>) -> HookOutput {
    let trigger = input
        .extra
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    tracing::info!(trigger, "Post-compaction state restoration");

    // Read active skill from session state
    let active_skill = ctx.fs.home_dir().and_then(|home| {
        let session_id = concrete_input_session_id(input)?;
        let state_path = home
            .join(".claude")
            .join("sentinel")
            .join("state")
            .join(format!("{session_id}.json"));
        let content = ctx.fs.read_to_string(&state_path).ok()?;
        let state: serde_json::Value = serde_json::from_str(&content).ok()?;
        state
            .get("active_skill")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
    });

    if let Some(skill) = &active_skill {
        let context = format!(
            "[Post-Compact Recovery] Context was compacted ({trigger}).\n\
             Active skill: {skill}. Reload phase files if needed.\n\
             Use Read(\"~/.claude/skills/{skill}/SKILL.md\") to restore context.",
        );
        HookOutput::inject_context(HookEvent::PostCompact, &context)
    } else {
        HookOutput::allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    #[test]
    fn test_post_compact_without_skill() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput::default();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn missing_session_does_not_consume_unknown_state() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let state_dir = tmp.path().join(".claude").join("sentinel").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("unknown.json"),
            r#"{"active_skill":"linear"}"#,
        )
        .unwrap();

        let output = process(&HookInput::default(), &ctx);

        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn synthetic_session_does_not_consume_unknown_state() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let state_dir = tmp.path().join(".claude").join("sentinel").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("unknown.json"),
            r#"{"active_skill":"linear"}"#,
        )
        .unwrap();
        let input = HookInput {
            session_id: Some(" unknown ".to_string()),
            ..Default::default()
        };

        let output = process(&input, &ctx);

        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn concrete_session_recovers_active_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = TestHomeFs::new(tmp.path());
        let ctx = stub_ctx_with_fs(&fs);
        let state_dir = tmp.path().join(".claude").join("sentinel").join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("post-compact-session.json"),
            r#"{"active_skill":"linear"}"#,
        )
        .unwrap();
        let input = HookInput {
            session_id: Some("post-compact-session".to_string()),
            ..Default::default()
        };

        let output = process(&input, &ctx);

        let context = output
            .hook_specific_output
            .and_then(|hook| hook.additional_context)
            .expect("active skill context");
        assert!(context.contains("Active skill: linear"));
    }
}
