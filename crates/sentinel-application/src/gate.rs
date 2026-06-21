//! Gate Evaluation
//!
//! Decides whether tool calls should be blocked based on workflow state,
//! proof chains, and custom gate rules.

use crate::hooks::FileSystemPort;
use sentinel_domain::events::HookInput;
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillWorkflow, WorkflowBlock, WorkflowState};

/// Result of evaluating a gate
#[derive(Debug)]
pub enum GateDecision {
    /// Allow the tool call
    Allow,

    /// Block the tool call with reason
    Block {
        /// The skill whose workflow actually produced this block. This is the
        /// `effective_skill` the gate evaluated (which may be an incomplete
        /// workflow found via `find_incomplete_workflow`), NOT necessarily
        /// `state.active_skill` — the two diverge when a workflow is enforced
        /// after the active skill was cleared/switched. The block message must
        /// use THIS so its `Read("~/.claude/skills/{skill}/phases/{file}")`
        /// remediation points at a real path, not `skills/unknown/...`.
        skill: String,
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
    // Check blocked_tool_prefixes across ALL workflows that have durable
    // LangGraph projection in this session. `active_skill` is only a context
    // marker for skill loading; it must not become workflow authority without a
    // checkpoint-backed graph state.
    for (wf_skill, wf_def) in workflows {
        if wf_def.blocked_tool_prefixes.is_empty() {
            continue;
        }
        if !state.has_graph_workflow(wf_skill) {
            continue;
        }
        for prefix in &wf_def.blocked_tool_prefixes {
            if tool_name.starts_with(prefix.as_str()) {
                let Some(next) = wf_def.phases.iter().find(|p| p.required) else {
                    return GateDecision::Block {
                        skill: wf_skill.clone(),
                        reason: format!(
                            "Workflow '{wf_skill}' has no required phases. Blocking because the workflow configuration is invalid."
                        ),
                        next_phase: String::new(),
                        next_phase_file: String::new(),
                    };
                };
                return GateDecision::Block {
                    skill: wf_skill.clone(),
                    reason: format!(
                        "Workflow '{wf_skill}': tool '{tool_name}' is blocked (matches blocked prefix '{prefix}').\n\
                         Use the workflow's native tools instead of equivalent alternatives."
                    ),
                    next_phase: next.id.clone(),
                    next_phase_file: next.file.clone(),
                };
            }
        }
    }

    // Phase gates are driven only by durable LangGraph workflow projection.
    // This still prevents skill-clear / skill-switch bypasses because projected
    // graph workflows remain in session state even when `active_skill` changes.
    let (workflow, workflow_state, effective_skill): (&SkillWorkflow, &WorkflowState, String) =
        match find_incomplete_workflow(state, workflows, None) {
            Some((ref_wf, ref_state, ref_skill)) => (ref_wf, ref_state, ref_skill),
            None => return GateDecision::Allow,
        };

    // Check if workflow blocks this tool
    if let Some(block) = workflow_state.should_block(workflow, tool_name) {
        return gate_block_decision(state, effective_skill, block, fs);
    }

    GateDecision::Allow
}

fn gate_block_decision(
    state: &SessionState,
    effective_skill: String,
    block: WorkflowBlock,
    fs: &dyn FileSystemPort,
) -> GateDecision {
    let next_phase_read = state.has_phase_been_read(&effective_skill, &block.next_phase_file);
    if next_phase_read {
        return GateDecision::Allow;
    }

    emit_phase_file_authority_warnings(&effective_skill, &block.next_phase_file, fs);

    GateDecision::Block {
        skill: effective_skill,
        reason: block.reason,
        next_phase: block.next_phase,
        next_phase_file: block.next_phase_file,
    }
}

fn emit_phase_file_authority_warnings(
    effective_skill: &str,
    next_phase_file: &str,
    fs: &dyn FileSystemPort,
) {
    let phases_dir = fs
        .home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude/skills")
        .join(effective_skill)
        .join("phases");

    let phase_file_path = phases_dir.join(next_phase_file);

    if !fs.exists(&phases_dir) {
        eprintln!(
            "[sentinel] WARNING: Phase directory missing for active workflow '{}': {}. \
             Blocking to fail closed.",
            effective_skill,
            phases_dir.display(),
        );
    } else if !fs.exists(&phase_file_path) {
        eprintln!(
            "[sentinel] WARNING: Phase file missing but phases/ dir exists: {}. \
             Skill '{}' has a broken phase configuration. Blocking to fail closed.",
            phase_file_path.display(),
            effective_skill,
        );
    }
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

    for (prev_skill, prev_state) in state.graph_workflows() {
        // Skip the excluded skill (e.g., the current active Tier 0 skill)
        if exclude_skill == Some(prev_skill) {
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
        // switch skill — a loose recovery path picks the decoy and enforces fewer remaining gates.
        // "Least progressed" = most remaining required phases = strictest enforcement.
        let progress = prev_state.completed_phases.len();
        if best
            .as_ref()
            .is_none_or(|(_, _, _, best_p)| progress < *best_p)
        {
            best = Some((prev_wf, prev_state, prev_skill.clone(), progress));
        }
    }

    best.map(|(wf, ws, skill, _)| (wf, ws, skill))
}

/// Public wrapper for `find_incomplete_workflow` — used by `phase_gate::check_post_merge_skip`
/// to mirror the skill-clear enforcement logic from the main gate evaluation.
pub fn find_incomplete_workflow_pub<'a>(
    state: &'a SessionState,
    workflows: &'a std::collections::HashMap<String, SkillWorkflow>,
    exclude_skill: Option<&String>,
) -> Option<(&'a SkillWorkflow, &'a WorkflowState, String)> {
    find_incomplete_workflow(state, workflows, exclude_skill)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
    };

    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::WorkflowPhase;

    use super::*;

    fn inject_old_workflows_field(
        state: &mut SessionState,
        skill: &str,
        workflow_state: WorkflowState,
    ) {
        let mut value = serde_json::to_value(&*state).expect("session state serializes");
        let old_workflows = serde_json::json!({
            skill: serde_json::to_value(workflow_state).expect("workflow state serializes")
        });
        value
            .as_object_mut()
            .expect("session state is an object")
            .insert("workflows".to_string(), old_workflows);
        *state = serde_json::from_value(value).expect("session state deserializes");
    }

    struct ExistingPhaseFs;

    impl FileSystemPort for ExistingPhaseFs {
        fn home_dir(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/tmp/sentinel-test-home"))
        }

        fn read_to_string(
            &self,
            _p: &Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            unimplemented!("gate tests do not read files")
        }

        fn write(
            &self,
            _p: &Path,
            _content: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            unimplemented!("gate tests do not write files")
        }

        fn create_dir_all(
            &self,
            _p: &Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            unimplemented!("gate tests do not create directories")
        }

        fn read_dir(
            &self,
            _p: &Path,
        ) -> Result<Vec<PathBuf>, sentinel_domain::port_errors::FileSystemError> {
            unimplemented!("gate tests do not read directories")
        }

        fn exists(&self, p: &Path) -> bool {
            let s = p.to_string_lossy();
            s.ends_with(".claude/skills/linear/phases")
                || s.ends_with(".claude/skills/linear/phases/claim.md")
        }

        fn is_dir(&self, p: &Path) -> bool {
            p.to_string_lossy()
                .ends_with(".claude/skills/linear/phases")
        }

        fn metadata(
            &self,
            _p: &Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            unimplemented!("gate tests do not inspect metadata")
        }

        fn append(
            &self,
            _p: &Path,
            _content: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            unimplemented!("gate tests do not append files")
        }
    }

    fn workflow() -> SkillWorkflow {
        SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![WorkflowPhase {
                id: "claim".to_string(),
                file: "claim.md".to_string(),
                required: true,
                judge: JudgeModel::Sonnet,
                description: "claim".to_string(),
                required_dyad: None,
            }],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    #[test]
    fn phase_read_allows_next_phase_work_without_marking_phase_complete() {
        let mut state = SessionState::new("sess");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "claim.md");
        state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess"));
        let workflows = HashMap::from([("linear".to_string(), workflow())]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };

        assert!(matches!(
            evaluate(&state, &workflows, &input, &ExistingPhaseFs),
            GateDecision::Allow
        ));
        assert!(
            state
                .graph_workflow("linear")
                .is_some_and(|wf| wf.completed_phases.is_empty()),
            "phase read must not mark completion"
        );
    }

    #[test]
    fn active_workflow_marker_without_graph_state_is_context_only() {
        let mut state = SessionState::new("sess");
        state.set_active_skill_marker("linear");
        let workflows = HashMap::from([("linear".to_string(), workflow())]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };

        assert!(matches!(
            evaluate(&state, &workflows, &input, &ExistingPhaseFs),
            GateDecision::Allow
        ));
        assert!(
            !state.has_any_graph_workflow(),
            "marker-only state must not synthesize graph workflow authority"
        );
    }

    #[test]
    fn phase_read_without_graph_projection_is_not_workflow_authority() {
        let mut state = SessionState::new("sess");
        state.set_active_skill_marker("linear");
        state.record_phase_read("linear", "claim.md");
        let workflows = HashMap::from([("linear".to_string(), workflow())]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };

        assert!(matches!(
            evaluate(&state, &workflows, &input, &ExistingPhaseFs),
            GateDecision::Allow
        ));
        assert!(
            !state.has_any_graph_workflow(),
            "reading a phase file must not synthesize LangGraph workflow state"
        );
    }

    #[test]
    fn graph_projected_incomplete_workflow_blocks_after_active_skill_cleared() {
        let mut state = SessionState::new("sess");
        state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess"));
        let workflows = HashMap::from([("linear".to_string(), workflow())]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };

        assert!(matches!(
            evaluate(&state, &workflows, &input, &ExistingPhaseFs),
            GateDecision::Block { skill, .. } if skill == "linear"
        ));
    }

    #[test]
    fn old_workflows_field_does_not_enforce_after_active_skill_cleared() {
        let mut state = SessionState::new("sess");
        inject_old_workflows_field(&mut state, "linear", WorkflowState::new("linear", "sess"));
        let workflows = HashMap::from([("linear".to_string(), workflow())]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };

        assert!(matches!(
            evaluate(&state, &workflows, &input, &ExistingPhaseFs),
            GateDecision::Allow
        ));
    }

    #[test]
    fn future_phase_read_does_not_unlock_next_phase() {
        let mut state = SessionState::new("sess");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "fetch.md");
        state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess"));
        let mut wf = workflow();
        wf.phases.push(WorkflowPhase {
            id: "fetch".to_string(),
            file: "fetch.md".to_string(),
            required: true,
            judge: JudgeModel::Sonnet,
            description: "fetch".to_string(),
            required_dyad: None,
        });
        let workflows = HashMap::from([("linear".to_string(), wf)]);
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };

        assert!(matches!(
            evaluate(&state, &workflows, &input, &ExistingPhaseFs),
            GateDecision::Block { .. }
        ));
    }
}
