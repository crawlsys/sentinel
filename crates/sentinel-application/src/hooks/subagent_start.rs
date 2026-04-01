//! SubagentStart hook — inject skill context into spawned agents
//!
//! When a subagent is spawned, injects the active skill context and
//! project configuration so the agent has relevant information.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};

/// Process SubagentStart event
///
/// Injects active skill and project context into the spawned agent.
pub fn process(input: &HookInput) -> HookOutput {
    let agent_type = input
        .extra
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Read active skill from session state if available
    let state_dir = dirs::home_dir()
        .map(|h| h.join(".claude").join("sentinel").join("state"));

    let active_skill = state_dir
        .as_ref()
        .and_then(|dir| {
            let session_id = input.session_id.as_deref()?;
            let state_path = dir.join(format!("{session_id}.json"));
            let content = std::fs::read_to_string(&state_path).ok()?;
            let state: serde_json::Value = serde_json::from_str(&content).ok()?;
            state
                .get("active_skill")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let context = if let Some(skill) = &active_skill {
        format!(
            "[Subagent Context] Agent type '{}' spawned during skill '{}'.\n\
             The parent session is executing the '{}' skill — align your work accordingly.",
            agent_type, skill, skill
        )
    } else {
        format!(
            "[Subagent Context] Agent type '{}' spawned.",
            agent_type
        )
    };

    HookOutput::inject_context(HookEvent::SubagentStart, &context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subagent_start_injects_context() {
        let mut input = HookInput::default();
        input
            .extra
            .insert("agent_type".to_string(), serde_json::json!("code-reviewer"));

        let output = process(&input);
        assert!(output.hook_specific_output.is_some());
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("code-reviewer"));
    }
}
