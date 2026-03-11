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
        let phases_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".claude/skills")
            .join(skill_name)
            .join("phases");

        let phase_file_path = phases_dir.join(&block.next_phase_file);

        // Distinguish two cases:
        // 1. "No phase system" — phases/ directory doesn't exist at all.
        //    The workflow definition in workflows.toml is documentation only.
        //    This prevents hard-gate deadlocks for 42+ skills without phase files.
        // 2. "Phase file missing" — phases/ dir exists but the specific file is gone.
        //    This is likely a configuration error. Log a loud warning but still block,
        //    because the skill HAS a phase system — it's just broken.
        if !phases_dir.exists() {
            // Case 1: No phase system authored yet — allow
            return GateDecision::Allow;
        }

        if !phase_file_path.exists() {
            // Case 2: Phase system exists but specific file is missing.
            // Log warning for debugging, but still block — fail closed.
            eprintln!(
                "[sentinel] WARNING: Phase file missing but phases/ dir exists: {}. \
                 Skill '{}' has a broken phase configuration. Blocking to fail closed.",
                phase_file_path.display(),
                skill_name,
            );
            // Still block — the skill has a phase system, the file is just missing.
            // The user needs to either create the file or remove the phases/ dir.
        }

        return GateDecision::Block {
            reason: block.reason,
            next_phase: block.next_phase,
            next_phase_file: block.next_phase_file,
        };
    }

    GateDecision::Allow
}
