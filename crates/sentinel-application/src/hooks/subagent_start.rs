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
    // Claude Code sends `agent_type` as a TOP-LEVEL SubagentStart field, which
    // serde routes into the typed `HookInput::agent_type` — never into the
    // `#[serde(flatten)]` extra map. Reading it from `extra` (the pre-fix code)
    // was therefore always empty, so every injection said "Agent type
    // 'unknown'". Read the typed field first, fall back to `extra` only for
    // resilience against a future harness that relocates it — the same pattern
    // as `subagent_stop::resolve_agent_identity`.
    let agent_type = input
        .agent_type
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            input
                .extra
                .get("agent_type")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
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
        // Real CC payload: agent_type on the TYPED field, not extra.
        let input = HookInput {
            agent_type: Some("code-reviewer".to_string()),
            ..Default::default()
        };

        let ctx = crate::hooks::test_support::stub_ctx();
        let output = process(&input, &ctx);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("code-reviewer"));
    }

    #[test]
    fn reads_typed_agent_type_not_extra() {
        // Regression: the typed field must be authoritative. A CC payload
        // deserializes agent_type into the typed field; extra stays empty.
        let payload = serde_json::json!({
            "hook_event_name": "SubagentStart",
            "agent_type": "debugger"
        });
        let input: HookInput = serde_json::from_value(payload).unwrap();
        assert!(input.extra.get("agent_type").is_none(), "typed field is not in extra");
        let ctx = crate::hooks::test_support::stub_ctx();
        let body = process(&input, &ctx)
            .hook_specific_output
            .unwrap()
            .additional_context
            .unwrap();
        assert!(body.contains("debugger"), "must read the typed agent_type");
        assert!(!body.contains("unknown"));
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
        let input = HookInput {
            session_id: Some(" unknown ".to_string()),
            agent_type: Some("code-reviewer".to_string()),
            ..Default::default()
        };

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
