//! Phase Validator Hook
//!
//! Runs on UserPromptSubmit and injects phase progress into the context.
//! Mirrors the Node.js phase-validator.js hook behavior:
//! - Shows current phase progress (e.g., "3/7 phases loaded")
//! - Warns when phases are being skipped
//! - Tells Claude which phase file to Read() next

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;
use std::collections::HashMap;

/// Process a phase-validator hook event (UserPromptSubmit)
pub fn process(
    _input: &HookInput,
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
) -> HookOutput {
    // Only act when there's an active skill with a workflow
    let skill_name = match &state.active_skill {
        Some(s) => s,
        None => return HookOutput::allow(),
    };

    let workflow = match workflows.get(skill_name) {
        Some(wf) => wf,
        None => return HookOutput::allow(),
    };

    let total_required = workflow.phases.iter().filter(|p| p.required).count();
    let total = workflow.phases.len();
    let loaded = state.phases_read_count();

    // Find next required phase file
    let next_file = state.next_required_phase_file(workflow);

    // Build progress context
    let context = if state.tool_calls > 0 && loaded == 0 {
        // Tool calls happening but no phases loaded — strong warning
        format!(
            "{}",
            format_warning_box(skill_name, total_required, total)
        )
    } else if let Some(ref next) = next_file {
        // Normal progress — show status and next step
        format_progress_box(skill_name, loaded, total_required, total, next)
    } else {
        // All required phases loaded
        format!(
            "[Phase Progress] Skill: {} | All {}/{} required phases loaded. Proceed with execution.",
            skill_name, total_required, total
        )
    };

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

/// Format a warning box when phases are being skipped
fn format_warning_box(skill: &str, required: usize, total: usize) -> String {
    format!(
        "\
+============================================================+
|  WARNING: Phase Execution Required                         |
+============================================================+
|  Skill: {:<50}|
|  Phases loaded: 0/{} (0/{} total)                          |
|                                                            |
|  You are making tool calls without loading ANY phase       |
|  files. This is a violation of the skill workflow.         |
|                                                            |
|  MANDATORY: Read the first phase file NOW:                 |
|  Read(\"~/.claude/skills/{}/phases/claim.md\")     |
+============================================================+",
        skill, required, total, skill
    )
}

/// Format a progress box showing current status
fn format_progress_box(
    skill: &str,
    loaded: usize,
    required: usize,
    total: usize,
    next_file: &str,
) -> String {
    // Build phase checklist
    format!(
        "[Phase Progress] Skill: {} | Phases loaded: {}/{} (required: {}) | \
         Next: Read(\"~/.claude/skills/{}/phases/{}\")",
        skill, loaded, total, required, skill, next_file
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::WorkflowPhase;

    fn test_workflow() -> SkillWorkflow {
        SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![
                WorkflowPhase {
                    id: "claim".to_string(),
                    file: "claim.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Claim the issue".to_string(),
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Fetch details".to_string(),
                },
                WorkflowPhase {
                    id: "intelligence".to_string(),
                    file: "intelligence.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Research codebase".to_string(),
                },
                WorkflowPhase {
                    id: "cleanup".to_string(),
                    file: "cleanup.md".to_string(),
                    required: false,
                    judge: JudgeModel::Sonnet,
                    description: "Cleanup".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_no_active_skill_passes_through() {
        let state = SessionState::new("sess-1");
        let workflows = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_progress_injection_with_no_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let input = HookInput::default();

        let output = process(&input, &state, &workflows);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("Phases loaded: 0/4"));
        assert!(ctx.contains("claim.md"));
    }

    #[test]
    fn test_progress_injection_with_some_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("claim.md");
        state.record_phase_read("fetch.md");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let input = HookInput::default();

        let output = process(&input, &state, &workflows);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("Phases loaded: 2/4"));
        assert!(ctx.contains("intelligence.md"));
    }

    #[test]
    fn test_warning_when_tool_calls_but_no_phases() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_tool_call();
        state.record_tool_call();

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let input = HookInput::default();

        let output = process(&input, &state, &workflows);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("WARNING"));
        assert!(ctx.contains("Phase Execution Required"));
    }

    #[test]
    fn test_all_required_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("claim.md");
        state.record_phase_read("fetch.md");
        state.record_phase_read("intelligence.md");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let input = HookInput::default();

        let output = process(&input, &state, &workflows);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        assert!(ctx.contains("All 3/4 required phases loaded"));
    }
}
