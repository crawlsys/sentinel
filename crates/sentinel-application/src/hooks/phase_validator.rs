//! Phase Validator Hook
//!
//! Runs on UserPromptSubmit and injects phase progress into the context.
//! Mirrors the Node.js phase-validator.js hook behavior:
//! - Shows current phase progress (e.g., "3/7 phases loaded")
//! - Shows step-level progress when step configs are available
//! - Warns when phases are being skipped
//! - Tells Claude which phase file to Read() next

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, SkillWorkflow, StepStatus};
use std::collections::HashMap;

/// Check whether a skill's `phases/` directory exists on disk under
/// `~/.claude/skills/{skill}/phases/`. When a workflow is registered but
/// the skill uses an alternate structure (e.g. `orchestration/`,
/// `protocols/`) the validator would otherwise emit a "Phase Execution
/// Required" warning referencing a nonexistent file.
fn skill_has_phases_dir(fs: &dyn super::FileSystemPort, skill: &str) -> bool {
    let Some(home) = fs.home_dir() else {
        return true; // Fail open — don't silently suppress warnings if we can't check.
    };
    fs.is_dir(
        &home
            .join(".claude")
            .join("skills")
            .join(skill)
            .join("phases"),
    )
}

/// Process a phase-validator hook event (UserPromptSubmit)
pub fn process(
    _input: &HookInput,
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    step_configs: &HashMap<String, SkillSteps>,
    fs: &dyn super::FileSystemPort,
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

    // If the skill has no phases/ directory on disk, it uses an alternate
    // structure and the workflow config is stale. Don't emit warnings
    // pointing at files that don't exist.
    if !skill_has_phases_dir(fs, skill_name) {
        return HookOutput::allow();
    }

    let total_required = workflow.phases.iter().filter(|p| p.required).count();
    let total = workflow.phases.len();
    // **Attack #76 fix**: Use per-skill count, not global. Cross-skill reads
    // inflated the counter, misleading Claude into thinking more phases were done.
    let loaded = state
        .phases_read
        .get(skill_name)
        .map(|v| v.len())
        .unwrap_or(0);

    // Find next required phase file
    let next_file = state.next_required_phase_file(workflow);

    // First required phase file, used by the warning box when phases
    // aren't being loaded at all. Falls back to `claim.md` only if the
    // workflow has no phases configured (shouldn't happen in practice).
    let first_phase_file = workflow
        .phases
        .iter()
        .find(|p| p.required)
        .or_else(|| workflow.phases.first())
        .map(|p| p.file.as_str())
        .unwrap_or("claim.md");

    // Build progress context
    let mut context = if state.tool_calls > 0 && loaded == 0 {
        // Tool calls happening but no phases loaded — strong warning
        format_warning_box(skill_name, total_required, total, first_phase_file)
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

    // Append step-level progress if step configs exist
    if let Some(steps_config) = step_configs.get(skill_name) {
        if let Some(wf_state) = state.workflows.get(skill_name) {
            let step_context = format_step_progress(wf_state, steps_config, workflow);
            if !step_context.is_empty() {
                context.push('\n');
                context.push_str(&step_context);
            }
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
|  Skill: {:<50}|
|  Phases loaded: 0/{} (0/{} total)                          |
|                                                            |
|  You are making tool calls without loading ANY phase       |
|  files. This is a violation of the skill workflow.         |
|                                                            |
|  MANDATORY: Read the first phase file NOW:                 |
|  Read(\"~/.claude/skills/{}/phases/{}\")     |
+============================================================+",
        skill, required, total, skill, first_file
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
            // Fall back to the next incomplete required phase
            wf_state.next_required_phase(workflow).map(|p| p.id.clone())
        });

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
                "  Phase: {} ({}/{} steps)",
                phase_id, phase_completed, phase_total
            ));

            // Show up to 5 steps for the current phase (context window friendly)
            let step_states = wf_state.phase_step_states(phase_id);
            let mut shown = 0;
            for step_def in &phase_steps.steps {
                if shown >= 5 {
                    let remaining = phase_total - shown;
                    if remaining > 0 {
                        lines.push(format!("    ... +{} more steps", remaining));
                    }
                    break;
                }

                let status_icon = step_states
                    .iter()
                    .find(|s| s.step_id == step_def.id)
                    .map(|s| match s.status {
                        StepStatus::Completed => "\u{2713}",  // checkmark
                        StepStatus::InProgress => "\u{2192}", // arrow
                        StepStatus::Skipped => "\u{2014}",    // em-dash
                        StepStatus::Blocked => "\u{2717}",    // X
                        StepStatus::Pending => "\u{25CB}",    // circle
                    })
                    .unwrap_or("\u{25CB}"); // default: circle (pending)

                let blocker_tag = if step_def.blocker { " [BLOCKER]" } else { "" };

                let summary = step_states
                    .iter()
                    .find(|s| s.step_id == step_def.id)
                    .and_then(|s| s.summary.as_deref())
                    .map(|s| format!(" \u{2014} {}", s))
                    .unwrap_or_default();

                lines.push(format!(
                    "    {} {}: {}{}{}",
                    status_icon, step_def.id, step_def.description, blocker_tag, summary
                ));
                shown += 1;
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

    /// Stub `FileSystemPort` whose `is_dir` always returns `true` so the
    /// `skill_has_phases_dir` check passes and the validator gets to emit
    /// progress / warning context. Real-disk structure isn't available
    /// during unit tests; production code already validates the path under
    /// `~/.claude/skills/{skill}/phases/`.
    struct PhasesExistFs;
    impl crate::hooks::FileSystemPort for PhasesExistFs {
        fn home_dir(&self) -> Option<std::path::PathBuf> {
            Some(std::path::PathBuf::from("/mock/home"))
        }
        fn read_to_string(&self, _: &std::path::Path) -> anyhow::Result<String> {
            anyhow::bail!("no")
        }
        fn write(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_dir_all(&self, _: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn read_dir(&self, _: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
            Ok(vec![])
        }
        fn exists(&self, _: &std::path::Path) -> bool {
            true
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            true
        }
        fn metadata(&self, _: &std::path::Path) -> anyhow::Result<std::fs::Metadata> {
            anyhow::bail!("no")
        }
        fn append(&self, _: &std::path::Path, _: &[u8]) -> anyhow::Result<()> {
            Ok(())
        }
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
    fn test_skill_without_phases_dir_suppresses_warning() {
        // A skill whose on-disk layout has no `phases/` directory should
        // NOT trigger the "Phase Execution Required" warning — the
        // workflow config is stale. Use a skill name guaranteed not to
        // exist on disk.
        let mut state = SessionState::new("sess-nope");
        state.set_active_skill("nonexistent-phantom-skill-xyz");
        state.record_tool_call();

        let mut workflows = HashMap::new();
        workflows.insert("nonexistent-phantom-skill-xyz".to_string(), test_workflow());
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
        // allow() means no context injection
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_all_required_phases_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "claim.md");
        state.record_phase_read("linear", "fetch.md");
        state.record_phase_read("linear", "intelligence.md");

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());
        let step_configs = HashMap::new();
        let input = HookInput::default();

        let output = process(&input, &state, &workflows, &step_configs, &PhasesExistFs);
        let ctx = output.hook_specific_output.unwrap().additional_context;
        let ctx = ctx.as_deref().unwrap();
        assert!(ctx.contains("All 3/4 required phases loaded"));
    }

    #[test]
    fn test_step_progress_injection() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("linear", "claim.md");

        // Update some steps in the workflow state
        if let Some(wf) = state.active_workflow_mut() {
            wf.update_step(
                "claim",
                "0.1",
                StepStatus::Completed,
                Some("Found state ID".to_string()),
            );
            wf.update_step("claim", "0.2", StepStatus::InProgress, None);
        }

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
