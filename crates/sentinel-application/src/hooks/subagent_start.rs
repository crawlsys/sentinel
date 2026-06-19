//! `SubagentStart` hook — inject skill context into spawned agents
//!
//! When a subagent is spawned, injects the active skill context and
//! project configuration so the agent has relevant information.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

use super::concrete_input_session_id;

/// Process `SubagentStart` event
///
/// Injects active skill and project context into the spawned agent.
pub fn process(input: &HookInput, ctx: &super::HookContext<'_>) -> HookOutput {
    let agent_type = input
        .extra
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Read active skill from session state if available
    let state_dir = ctx
        .fs
        .home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("state"));

    let active_skill = state_dir.as_ref().and_then(|dir| {
        let session_id = concrete_input_session_id(input)?;
        let state_path = dir.join(format!("{session_id}.json"));
        let content = ctx.fs.read_to_string(&state_path).ok()?;
        let state: serde_json::Value = serde_json::from_str(&content).ok()?;
        state
            .get("active_skill")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string)
    });

    let context = if let Some(skill) = &active_skill {
        format!(
            "[Subagent Context] Agent type '{agent_type}' spawned during skill '{skill}'.\n\
             The parent session is executing the '{skill}' skill — align your work accordingly."
        )
    } else {
        format!("[Subagent Context] Agent type '{agent_type}' spawned.")
    };

    HookOutput::inject_context(HookEvent::SubagentStart, &context)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::{stub_ctx_with_fs, TestHomeFs};

    #[test]
    fn test_subagent_start_injects_context() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("agent_type".to_string(), serde_json::json!("code-reviewer"));

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("code-reviewer"));
    }

    #[test]
    fn synthetic_session_does_not_load_unknown_active_skill() {
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
        let mut input = HookInput {
            session_id: Some(" unknown ".to_string()),
            ..Default::default()
        };
        input
            .extra
            .insert("agent_type".to_string(), serde_json::json!("code-reviewer"));

        let output = process(&input, &ctx);

        let context = output
            .hook_specific_output
            .unwrap()
            .additional_context
            .unwrap();
        assert!(context.contains("code-reviewer"));
        assert!(!context.contains("skill 'linear'"));
    }
}
