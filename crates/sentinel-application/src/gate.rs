//! Gate Evaluation
//!
//! Decides whether tool calls should be blocked based on workflow state,
//! proof chains, and custom gate rules.

use sentinel_domain::events::HookInput;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;

/// Result of evaluating a gate
#[derive(Debug)]
pub enum GateDecision {
    /// Allow the tool call
    Allow,

    /// Block the tool call with reason
    Block {
        reason: String,
        next_phase: String,
        next_phase_file: String,
    },
}

/// Evaluate whether a tool call should be gated
pub fn evaluate(
    state: &SessionState,
    workflows: &std::collections::HashMap<String, SkillWorkflow>,
    input: &HookInput,
) -> GateDecision {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return GateDecision::Allow,
    };

    // Check active skill's workflow
    let skill_name = match &state.active_skill {
        Some(s) => s,
        None => return GateDecision::Allow,
    };

    let workflow = match workflows.get(skill_name) {
        Some(wf) => wf,
        None => return GateDecision::Allow,
    };

    let workflow_state = match state.workflows.get(skill_name) {
        Some(ws) => ws,
        None => return GateDecision::Allow,
    };

    // Check if workflow blocks this tool
    if let Some(block) = workflow_state.should_block(workflow, tool_name) {
        // Only enforce the gate when the required phase file exists on disk.
        // If phase files aren't authored yet, the workflow definition in
        // workflows.toml is documentation — not enforceable. This prevents
        // hard-gate deadlocks for the 42+ skills without phase files.
        let phase_file_path = dirs::home_dir()
            .unwrap_or_default()
            .join(".claude/skills")
            .join(skill_name)
            .join("phases")
            .join(&block.next_phase_file);

        if !phase_file_path.exists() {
            return GateDecision::Allow;
        }

        return GateDecision::Block {
            reason: block.reason,
            next_phase: block.next_phase,
            next_phase_file: block.next_phase_file,
        };
    }

    GateDecision::Allow
}
