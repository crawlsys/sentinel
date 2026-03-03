//! Phase Gate Hook
//!
//! Blocks tool calls when skill phases are skipped.
//! Uses the workflow state machine to determine if a tool should be blocked.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;
use std::collections::HashMap;

/// Process a phase-gate hook event
pub fn process(
    input: &HookInput,
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
) -> HookOutput {
    let result = crate::gate::evaluate(state, workflows, input);
    match result {
        crate::gate::GateDecision::Allow => HookOutput::allow(),
        crate::gate::GateDecision::Block {
            reason,
            next_phase,
            next_phase_file,
        } => {
            let message = format!(
                "BLOCKED: {}\n\n\
                 Next step: Read the phase file at \
                 ~/.claude/skills/linear/phases/{}\n\
                 Then complete the '{}' phase before proceeding.",
                reason, next_phase_file, next_phase
            );
            HookOutput::block(message)
        }
    }
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
                    id: "implement".to_string(),
                    file: "implement.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Implement the solution".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_allows_when_no_active_skill() {
        let state = SessionState::new("sess-1");
        let workflows = HashMap::new();
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &state, &workflows);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_safe_tools() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            ..Default::default()
        };
        let output = process(&input, &state, &workflows);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_blocks_when_no_phases_completed() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &state, &workflows);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.unwrap().contains("claim"));
    }
}
