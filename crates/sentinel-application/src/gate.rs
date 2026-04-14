//! Gate Evaluation
//!
//! Decides whether tool calls should be blocked based on workflow state,
//! proof chains, and custom gate rules.

use crate::hooks::FileSystemPort;
use sentinel_domain::events::HookInput;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillWorkflow, WorkflowState};

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
    fs: &dyn FileSystemPort,
) -> GateDecision {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return GateDecision::Allow,
    };

    // ── Cross-workflow blocked tool prefix check ──────────────────────
    // Check blocked_tool_prefixes across ALL workflows that have been activated
    // in this session (have workflow state) or are the current active skill.
    // This prevents the skill-switch bypass: switch to skill B (no blocked
    // prefixes), use tools that skill A blocks, switch back.
    for (wf_skill, wf_def) in workflows {
        if wf_def.blocked_tool_prefixes.is_empty() {
            continue;
        }
        // Only enforce for workflows that have been touched in this session
        let is_relevant = state.active_skill.as_deref() == Some(wf_skill)
            || state.workflows.contains_key(wf_skill);
        if !is_relevant {
            continue;
        }
        for prefix in &wf_def.blocked_tool_prefixes {
            if tool_name.starts_with(prefix.as_str()) {
                let next = wf_def
                    .phases
                    .iter()
                    .find(|p| p.required)
                    .map(|p| (p.id.clone(), p.file.clone()));
                let (next_phase, next_file) = next.unwrap_or_default();
                return GateDecision::Block {
                    reason: format!(
                        "Workflow '{}': tool '{}' is blocked (matches blocked prefix '{}').\n\
                         Use the workflow's native tools instead of equivalent alternatives.",
                        wf_skill, tool_name, prefix
                    ),
                    next_phase,
                    next_phase_file: next_file,
                };
            }
        }
    }

    // Check active skill's workflow for phase-based gating.
    //
    // ── Skill-clear / skill-switch bypass prevention (Attacks #24/#38) ──
    // Three cases:
    //   1. active_skill has a workflow → enforce it directly
    //   2. active_skill is set but has no workflow (Tier 0 skill) → fall through
    //      to incomplete workflow check
    //   3. active_skill is None (skill cleared by "no match" or general chat) →
    //      fall through to incomplete workflow check
    //
    // Cases 2 and 3 both search for previously-activated incomplete workflows.
    // This prevents: activate steel → say "hello" (clears skill) → all tools pass.
    let (workflow, workflow_state, effective_skill) = match &state.active_skill {
        Some(skill_name) => match workflows.get(skill_name.as_str()) {
            Some(wf) => match state.workflows.get(skill_name.as_str()) {
                Some(ws) => (wf, ws, skill_name.clone()),
                None => return GateDecision::Allow,
            },
            None => {
                // Active skill has no workflow definition (Tier 0) — fall through
                match find_incomplete_workflow(state, workflows, Some(skill_name)) {
                    Some((ref_wf, ref_state, ref_skill)) => (ref_wf, ref_state, ref_skill),
                    None => return GateDecision::Allow,
                }
            }
        },
        None => {
            // No active skill — but may have incomplete workflows from earlier.
            // Attack #38: "say hello" clears active_skill via skill router.
            match find_incomplete_workflow(state, workflows, None) {
                Some((ref_wf, ref_state, ref_skill)) => (ref_wf, ref_state, ref_skill),
                None => return GateDecision::Allow,
            }
        }
    };

    // Check if workflow blocks this tool
    if let Some(block) = workflow_state.should_block(workflow, tool_name) {
        let phases_dir = fs
            .home_dir()
            .expect("[sentinel] FATAL: Cannot determine home directory")
            .join(".claude/skills")
            .join(&effective_skill)
            .join("phases");

        let phase_file_path = phases_dir.join(&block.next_phase_file);

        // Distinguish two cases:
        // 1. "No phase system" — phases/ directory doesn't exist at all.
        //    The workflow definition in workflows.toml is documentation only.
        //    This prevents hard-gate deadlocks for 42+ skills without phase files.
        // 2. "Phase file missing" — phases/ dir exists but the specific file is gone.
        //    This is likely a configuration error. Log a loud warning but still block,
        //    because the skill HAS a phase system — it's just broken.
        if !fs.exists(&phases_dir) {
            // Case 1: No phase system authored yet — allow
            return GateDecision::Allow;
        }

        if !fs.exists(&phase_file_path) {
            // Case 2: Phase system exists but specific file is missing.
            // Log warning for debugging, but still block — fail closed.
            eprintln!(
                "[sentinel] WARNING: Phase file missing but phases/ dir exists: {}. \
                 Skill '{}' has a broken phase configuration. Blocking to fail closed.",
                phase_file_path.display(),
                effective_skill,
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

/// Find the most-progressed incomplete workflow from a previous skill activation.
///
/// Searches all workflow states in the session for workflows that:
///   1. Have a workflow definition in config (not a Tier 0 skill)
///   2. Have at least one required phase remaining (not fully complete)
///
/// Returns the workflow with the most completed phases (highest investment),
/// so clearing `active_skill` to a Tier 0 skill or `None` still enforces
/// the gates of the most advanced incomplete workflow.
///
/// `exclude_skill` optionally skips a skill name from the search (used when
/// the active skill itself has no workflow and we don't want to match it).
fn find_incomplete_workflow<'a>(
    state: &'a SessionState,
    workflows: &'a std::collections::HashMap<String, SkillWorkflow>,
    exclude_skill: Option<&String>,
) -> Option<(&'a SkillWorkflow, &'a WorkflowState, String)> {
    let mut best: Option<(&'a SkillWorkflow, &'a WorkflowState, String, usize)> = None;

    for (prev_skill, prev_state) in &state.workflows {
        // Skip the excluded skill (e.g., the current active Tier 0 skill)
        if exclude_skill.map_or(false, |ex| ex == prev_skill) {
            continue;
        }

        // Must have a workflow definition
        let prev_wf = match workflows.get(prev_skill.as_str()) {
            Some(wf) => wf,
            None => continue,
        };

        // Must have at least one required phase remaining
        if prev_state.next_required_phase(prev_wf).is_none() {
            continue;
        }

        // **Attack #55 fix**: Pick the workflow with the LEAST progress (most restrictive).
        // "Most progressed" lets an attacker create a near-complete decoy workflow then
        // switch skill — the fallback picks the decoy and enforces fewer remaining gates.
        // "Least progressed" = most remaining required phases = strictest enforcement.
        let progress = prev_state.completed_phases.len();
        if best
            .as_ref()
            .map_or(true, |(_, _, _, best_p)| progress < *best_p)
        {
            best = Some((prev_wf, prev_state, prev_skill.clone(), progress));
        }
    }

    best.map(|(wf, ws, skill, _)| (wf, ws, skill))
}

/// Public wrapper for `find_incomplete_workflow` — used by `phase_gate::check_post_merge_skip`
/// to mirror the skill-clear fallback logic from the main gate evaluation.
pub fn find_incomplete_workflow_pub<'a>(
    state: &'a SessionState,
    workflows: &'a std::collections::HashMap<String, SkillWorkflow>,
    exclude_skill: Option<&String>,
) -> Option<(&'a SkillWorkflow, &'a WorkflowState, String)> {
    find_incomplete_workflow(state, workflows, exclude_skill)
}
