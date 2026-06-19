//! Phase Validator Hook
//!
//! Runs on `UserPromptSubmit` and injects phase progress into the context.
//! The validator follows Sentinel's LangGraph projection: if an active graph
//! workflow exists, it remains visible even when the transient `active_skill`
//! marker was cleared or switched. Marker-only configured workflows are
//! context-only and must not surface workflow authority.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, SkillWorkflow, StepStatus, WorkflowState};
use std::collections::HashMap;

struct ResolvedWorkflow<'a> {
    skill_name: String,
    workflow: &'a SkillWorkflow,
    workflow_state: Option<&'a WorkflowState>,
}

fn resolve_effective_workflow<'a>(
    state: &'a SessionState,
    workflows: &'a HashMap<String, SkillWorkflow>,
) -> Option<ResolvedWorkflow<'a>> {
    if let Some(skill_name) = state.active_skill.as_deref() {
        if let (Some(workflow), Some(workflow_state)) =
            (workflows.get(skill_name), state.graph_workflow(skill_name))
        {
            return Some(ResolvedWorkflow {
                skill_name: skill_name.to_string(),
                workflow,
                workflow_state: Some(workflow_state),
            });
        }
    }

    crate::gate::find_incomplete_workflow_pub(state, workflows, None).map(
        |(workflow, workflow_state, skill_name)| ResolvedWorkflow {
            skill_name,
            workflow,
            workflow_state: Some(workflow_state),
        },
    )
}

fn phase_dir_authority_context(fs: &dyn super::FileSystemPort, skill: &str) -> Option<String> {
    let Some(home) = fs.home_dir() else {
        return Some(format!(
            "{}[Phase Validator] Cannot determine the home directory for skill '{skill}'. \
             LangGraph phase authority cannot prove the phase-file root; fix filesystem \
             configuration before treating this workflow as executable.",
            HookOutput::SENTINEL_AUTHORITY_PREFIX
        ));
    };

    let phases_dir = home
        .join(".claude")
        .join("skills")
        .join(skill)
        .join("phases");
    if fs.is_dir(&phases_dir) {
        return None;
    }

    Some(format!(
        "{}[Phase Validator] Configured workflow '{skill}' points to a missing phase directory: {}. \
         This is stale or broken workflow configuration; do not suppress phase enforcement.",
        HookOutput::SENTINEL_AUTHORITY_PREFIX,
        phases_dir.display()
    ))
}

/// Process a phase-validator hook event (`UserPromptSubmit`)
#[allow(clippy::implicit_hasher)] // call sites in hook_cmd.rs are outside edit scope
pub fn process(
    _input: &HookInput,
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    step_configs: &HashMap<String, SkillSteps>,
    fs: &dyn super::FileSystemPort,
) -> HookOutput {
    let Some(resolved) = resolve_effective_workflow(state, workflows) else {
        return HookOutput::allow();
    };
    let skill_name = resolved.skill_name.as_str();
    let workflow = resolved.workflow;

    if let Some(context) = phase_dir_authority_context(fs, skill_name) {
        return HookOutput::inject_context(HookEvent::UserPromptSubmit, context);
    }

    let Some(wf_state) = resolved.workflow_state else {
        return HookOutput::allow();
    };

    let total_required = workflow.phases.iter().filter(|p| p.required).count();
    let total = workflow.phases.len();
    // **Attack #76 fix**: Use per-skill count, not global. Cross-skill reads
    // inflated the counter, misleading Claude into thinking more phases were done.
    let loaded = state
        .phases_read
        .get(skill_name)
        .map_or(0, std::vec::Vec::len);
    let graph_completed = wf_state.completed_phases.len();

    // Find next required phase file
    let next_file = state.next_required_phase_file(workflow);
    let graph_next = wf_state.next_required_phase(workflow);

    // First required phase file, used by the warning box when phases
    // aren't being loaded at all.
    let first_phase_file = workflow
        .phases
        .iter()
        .find(|p| p.required)
        .or_else(|| workflow.phases.first())
        .map_or("claim.md", |p| p.file.as_str());

    // Build progress context
    let mut context = if state.tool_calls > 0 && loaded == 0 && graph_completed == 0 {
        // Tool calls happening but no phases loaded — strong warning
        format_warning_box(skill_name, total_required, total, first_phase_file)
    } else if let Some(ref next) = next_file {
        // Normal progress — show status and next step
        format_progress_box(
            skill_name,
            loaded,
            total_required,
            total,
            graph_completed,
            next,
        )
    } else if let Some(next) = graph_next {
        format!(
            "[Phase Progress] Skill: {skill_name} | All {total_required}/{total} required phase \
             files loaded. LangGraph sealed: {graph_completed}/{total_required}. Next graph \
             phase awaiting proof: {}.",
            next.id
        )
    } else {
        // All required phases loaded and sealed by graph authority.
        format!(
            "[Phase Progress] Skill: {skill_name} | All {total_required}/{total} required phase \
             files loaded. LangGraph sealed: {graph_completed}/{total_required}. Proceed with execution."
        )
    };

    // Append step-level progress if step configs exist
    if let Some(steps_config) = step_configs.get(skill_name) {
        let step_context = format_step_progress(wf_state, steps_config, workflow);
        if !step_context.is_empty() {
            context.push('\n');
            context.push_str(&step_context);
        }
    }

    HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
}

/// Format a warning box when phases are being skipped
fn format_warning_box(skill: &str, required: usize, total: usize, first_file: &str) -> String {
    format!(
        "\
+============================================================+
|  WARNING: Phase Execution Required                         |
+============================================================+
|  Skill: {skill:<50}|
|  Phases loaded: 0/{required} (0/{total} total)                          |
|                                                            |
|  You are making tool calls without loading ANY phase       |
|  files. This is a violation of the skill workflow.         |
|                                                            |
|  MANDATORY: Read the first phase file NOW:                 |
|  Read(\"~/.claude/skills/{skill}/phases/{first_file}\")     |
+============================================================+"
    )
}

/// Format a progress box showing current status
fn format_progress_box(
    skill: &str,
    loaded: usize,
    required: usize,
    total: usize,
    graph_completed: usize,
    next_file: &str,
) -> String {
    // Build phase checklist
    format!(
        "[Phase Progress] Skill: {skill} | Phases loaded: {loaded}/{total} (required: {required}) | \
         LangGraph sealed: {graph_completed}/{required} | Next: Read(\"~/.claude/skills/{skill}/phases/{next_file}\")"
    )
}

/// Format step-level progress from workflow state + step config
fn format_step_progress(
    wf_state: &sentinel_domain::workflow::WorkflowState,
    steps_config: &SkillSteps,
    workflow: &SkillWorkflow,
) -> String {
    let total_steps = steps_config.total_steps();
    if total_steps == 0 {
        return String::new();
    }

    let completed = wf_state.total_steps_completed();

    // Find the current phase (based on what's in progress)
    let current_phase_id = wf_state
        .current_step
        .as_ref()
        .and_then(|step_id| {
            wf_state
                .step_states
                .iter()
                .find(|s| s.step_id == *step_id)
                .map(|s| s.phase_id.clone())
        })
        .or_else(|| {
            // Use the next incomplete required phase from graph state.
            wf_state.next_required_phase(workflow).map(|p| p.id.clone())
        });

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    // Percentages from usize→f64→u32: precision loss negligible for display; value is always 0..=100
    let pct = if total_steps > 0 {
        (completed as f64 / total_steps as f64 * 100.0) as u32
    } else {
        0
    };

    let mut lines = vec![format!(
        "[Step Progress] Overall: {}/{} steps ({}%)",
        completed, total_steps, pct
    )];

    // Show detail for the current phase
    if let Some(ref phase_id) = current_phase_id {
        if let Some(phase_steps) = steps_config.phase_steps(phase_id) {
            let phase_completed = wf_state.phase_steps_completed(phase_id);
            let phase_total = phase_steps.steps.len();

            lines.push(format!(
                "  Phase: {phase_id} ({phase_completed}/{phase_total} steps)"
            ));

            // Show up to 5 steps for the current phase (context window friendly)
            let step_states = wf_state.phase_step_states(phase_id);
            for (shown, step_def) in phase_steps.steps.iter().enumerate() {
                if shown >= 5 {
                    let remaining = phase_total - shown;
                    if remaining > 0 {
                        lines.push(format!("    ... +{remaining} more steps"));
                    }
                    break;
                }

                let status_icon = step_states
                    .iter()
                    .find(|s| s.step_id == step_def.id)
                    .map_or("\u{25CB}", |s| match s.status {
                        StepStatus::Completed => "\u{2713}",  // checkmark
                        StepStatus::InProgress => "\u{2192}", // arrow
                        StepStatus::Skipped => "\u{2014}",    // em-dash
                        StepStatus::Blocked => "\u{2717}",    // X
                        StepStatus::Pending => "\u{25CB}",    // circle
                    }); // default: circle (pending)

                let blocker_tag = if step_def.blocker { " [BLOCKER]" } else { "" };

                let summary = step_states
                    .iter()
                    .find(|s| s.step_id == step_def.id)
                    .and_then(|s| s.summary.as_deref())
                    .map(|s| format!(" \u{2014} {s}"))
                    .unwrap_or_default();

                lines.push(format!(
                    "    {} {}: {}{}{}",
                    status_icon, step_def.id, step_def.description, blocker_tag, summary
                ));
            }
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::judge::JudgeModel;
    use sentinel_domain::workflow::{PhaseSteps, WorkflowPhase, WorkflowStep};

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
                    required_dyad: None,
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Fetch details".to_string(),
                    required_dyad: None,
                },
                WorkflowPhase {
                    id: "intelligence".to_string(),
                    file: "intelligence.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: "Research codebase".to_string(),
                    required_dyad: None,
                },
                WorkflowPhase {
                    id: "cleanup".to_string(),
                    file: "cleanup.md".to_string(),
                    required: false,
                    judge: JudgeModel::Sonnet,
                    description: "Cleanup".to_string(),
                    required_dyad: None,
                },
            ],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        }
    }

    fn test_steps() -> SkillSteps {
        SkillSteps {
            skill: "linear".to_string(),
            federation_version: "1".to_string(),
            phases: vec![
                PhaseSteps {
                    phase_id: "claim".to_string(),
                    steps: vec![
                        WorkflowStep {
                            id: "0.1".to_string(),
                            description: "Look up started state".to_string(),
                            blocker: false,
                            baseline_threshold: 0,
                            judge: None,
                            timeout_ms: None,
                            retry_policy: Default::default(),
                            circuit_breaker: Default::default(),
                            provides: Vec::new(),
                            requires: Vec::new(),
                            external: Vec::new(),
                            inaccessible: false,
                            deprecated: None,
                            r#override: None,
                            extra: serde_json::Value::Null,
                        },
                        WorkflowStep {
                            id: "0.2".to_string(),
                            description: "Get current user".to_string(),
                            blocker: false,
                            baseline_threshold: 0,
                            judge: None,
                            timeout_ms: None,
                            retry_policy: Default::default(),
                            circuit_breaker: Default::default(),
                            provides: Vec::new(),
                            requires: Vec::new(),
                            external: Vec::new(),
                            inaccessible: false,
                            deprecated: None,
                            r#override: None,
                            extra: serde_json::Value::Null,
                        },
                        WorkflowStep {
                            id: "0.3".to_string(),
                            description: "Set In Progress".to_string(),
                            blocker: true,
                            baseline_threshold: 0,
                            judge: None,
                            timeout_ms: None,
                            retry_policy: Default::default(),
                            circuit_breaker: Default::default(),
                            provides: Vec::new(),
                            requires: Vec::new(),
                            external: Vec::new(),
                            inaccessible: false,
                            deprecated: None,
                            r#override: None,
                            extra: serde_json::Value::Null,
                        },
                    ],
                },
                PhaseSteps {
                    phase_id: "fetch".to_string(),
                    steps: vec![
                        WorkflowStep {
                            id: "1.1".to_string(),
                            description: "Get issue".to_string(),
                            blocker: false,
                            baseline_threshold: 0,
                            judge: None,
                            timeout_ms: None,
                            retry_policy: Default::default(),
                            circuit_breaker: Default::default(),
                            provides: Vec::new(),
                            requires: Vec::new(),
                            external: Vec::new(),
                            inaccessible: false,
                            deprecated: None,
                            r#override: None,
                            extra: serde_json::Value::Null,
                        },
                        WorkflowStep {
                            id: "1.2".to_string(),
                            description: "Get comments".to_string(),
                            blocker: false,
                            baseline_threshold: 0,
                            judge: None,
                            timeout_ms: None,
                            retry_policy: Default::default(),
                            circuit_breaker: Default::default(),
                            provides: Vec::new(),
                            requires: Vec::new(),
                            external: Vec::new(),
                            inaccessible: false,
                            deprecated: None,
                            r#override: None,
                            extra: serde_json::Value::Null,
                        },
                    ],
                },
            ],
        }
    }

    /// Stub `FileSystemPort` whose `is_dir` always returns `true` so validator
    /// tests can focus on graph/progress behavior without relying on real disk
    /// skill fixtures.
    struct PhasesExistFs;
    impl crate::hooks::FileSystemPort for PhasesExistFs {
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            Some(std::path::PathBuf::from("/mock/home"))
        }
        fn read_to_string(
            &self,
            _: &std::path::Path,
        ) -> Result<String, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::Backend(
                "no".into(),
            ))
        }
        fn write(
            &self,
            _: &std::path::Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
        fn create_dir_all(
            &self,
            _: &std::path::Path,
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
        fn read_dir(
            &self,
            _: &std::path::Path,
        ) -> Result<Vec<std::path::PathBuf>, sentinel_domain::port_errors::FileSystemError>
        {
            Ok(vec![])
        }
        fn exists(&self, _: &std::path::Path) -> bool {
            true
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            true
        }
        fn metadata(
            &self,
            _: &std::path::Path,
        ) -> Result<std::fs::Metadata, sentinel_domain::port_errors::FileSystemError> {
            Err(sentinel_domain::port_errors::FileSystemError::Backend(
                "no".into(),
            ))
        }
        fn append(
            &self,
            _: &std::path::Path,
            _: &[u8],
        ) -> Result<(), sentinel_domain::port_errors::FileSystemError> {
            Ok(())
        }
    }

    fn project_linear_graph(state: &mut SessionState) {
        state.set_graph_projected_workflow(
            "linear",
            sentinel_domain::workflow::WorkflowState::new("linear", state.session_id.clone()),
        );
    }

    #[test]
    fn test_no_active_skill_passes_through() {
        let state = SessionState::new("sess-1");
        let workflows = HashMap::new();
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_progress_injection_with_no_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        project_linear_graph(&mut state);
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("Phases loaded: 0/4"));
        assert!(ctx.contains("claim.md"));
    }

    #[test]
    fn test_progress_injection_with_some_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        project_linear_graph(&mut state);
        state.record_phase_read("linear", "claim.md");
        state.record_phase_read("linear", "fetch.md");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("Phases loaded: 2/4"));
        assert!(ctx.contains("intelligence.md"));
    }

    #[test]
    fn test_warning_when_tool_calls_but_no_phases() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        project_linear_graph(&mut state);
        state.record_tool_call();
        state.record_tool_call();

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("WARNING"));
        assert!(ctx.contains("Phase Execution Required"));
    }

    #[test]
    fn marker_only_configured_workflow_is_context_only() {
        let mut state = SessionState::new("sess-marker-only");
        state.set_active_skill("linear");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        assert!(output.hook_specific_output.is_none());
        assert!(
            !state.has_any_graph_workflow(),
            "phase validator must not synthesize LangGraph workflow authority"
        );
    }

    #[test]
    fn graph_workflow_is_reported_when_active_skill_is_cleared() {
        let mut state = SessionState::new("sess-graph-only");
        project_linear_graph(&mut state);
        state.record_phase_read("linear", "claim.md");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("Skill: linear"));
        assert!(ctx.contains("Phases loaded: 1/4"));
    }

    #[test]
    fn test_skill_without_phases_dir_injects_authority_warning() {
        let mut state = SessionState::new("sess-nope");
        state.set_active_skill("nonexistent-phantom-skill-xyz");
        state.set_graph_projected_workflow(
            "nonexistent-phantom-skill-xyz",
            sentinel_domain::workflow::WorkflowState::new(
                "nonexistent-phantom-skill-xyz",
                "sess-nope",
            ),
        );
        state.record_tool_call();

        let mut workflows = HashMap::new();
        let mut workflow = test_workflow();
        workflow.skill = "nonexistent-phantom-skill-xyz".to_string();
        workflows.insert("nonexistent-phantom-skill-xyz".to_string(), workflow);
        let step_configs = HashMap::new();
        let input = HookInput::default();

        // StubFs.is_dir returns false — simulates "no phases/ directory on disk".
        let output = process(
            &input,
            &state,
            &workflows,
            &step_configs,
            &crate::hooks::test_support::StubFs,
        );
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains(HookOutput::SENTINEL_AUTHORITY_PREFIX));
        assert!(ctx.contains("missing phase directory"));
        assert!(ctx.contains("do not suppress phase enforcement"));
    }

    #[test]
    fn test_all_required_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "claim.md");
        state.record_phase_read("linear", "fetch.md");
        state.record_phase_read("linear", "intelligence.md");

        let workflow = test_workflow();
        let mut workflow_state =
            sentinel_domain::workflow::WorkflowState::new("linear", state.session_id.clone());
        workflow_state.completed_phases = vec![
            "claim".to_string(),
            "fetch".to_string(),
            "intelligence".to_string(),
        ];
        workflow_state.complete = true;
        state.set_graph_projected_workflow("linear", workflow_state);

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), workflow);
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("All 3/4 required phase"));
        assert!(ctx.contains("LangGraph sealed: 3/3"));
    }

    #[test]
    fn test_step_progress_injection() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "claim.md");

        let mut workflow_state = sentinel_domain::workflow::WorkflowState::new("linear", "sess-1");
        workflow_state.update_step(
            "claim",
            "0.1",
            StepStatus::Completed,
            Some("Found state ID".to_string()),
        );
        workflow_state.update_step("claim", "0.2", StepStatus::InProgress, None);
        state.set_graph_projected_workflow("linear", workflow_state);

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let mut step_configs = HashMap::new();
        step_configs.insert("linear".to_string(), test_steps());
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();

        // Should contain step progress line
        assert!(ctx.contains("[Step Progress]"));
        assert!(ctx.contains("Overall: 1/5 steps (20%)"));
        // Should show claim phase detail
        assert!(ctx.contains("Phase: claim (1/3 steps)"));
        // Should show individual steps
        assert!(ctx.contains("0.1: Look up started state"));
        assert!(ctx.contains("0.3: Set In Progress"));
        assert!(ctx.contains("[BLOCKER]"));
    }

    #[test]
    fn test_step_progress_without_config() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        project_linear_graph(&mut state);
        state.record_phase_read("linear", "claim.md");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        // No step configs — should produce normal output without step details
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();

        // Should NOT contain step progress
        assert!(!ctx.contains("[Step Progress]"));
        // Should still contain normal phase progress
        assert!(ctx.contains("Phases loaded: 1/4"));
    }
}
