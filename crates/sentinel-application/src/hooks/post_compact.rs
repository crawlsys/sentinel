//! `PostCompact` hook — restore critical state after context compaction
//!
//! Called after compaction completes. Receives `compact_summary`.
//! Can inject additionalContext to restore critical information.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::HookContext;

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
        let session_id = input.session_id.as_deref()?;
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

    #[test]
    fn test_post_compact_without_skill() {
        let ctx = crate::hooks::test_support::stub_ctx();
        let input = HookInput::default();
        let output = process(&input, &ctx);
        assert!(output.blocked.is_none());
    }
}
