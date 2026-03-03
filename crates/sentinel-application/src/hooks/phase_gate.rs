//! Phase Gate Hook
//!
//! Blocks tool calls when skill phases are skipped.
//! Uses the workflow state machine to determine if a tool should be blocked.
//!
//! Enhanced features (ported from Node.js phase-gate.js):
//! - Tracks Read() calls on phase files via SessionState
//! - Formatted block messages with visual boxes
//! - Post-merge skip detection (review done but qa-handoff not loaded)
//! - Allows tools within 1 phase gap (mid-phase), blocks at 2+ gap

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::SkillWorkflow;
use std::collections::HashMap;

/// Extract the phase file name from a Read() tool_input path.
/// Matches paths like `~/.claude/skills/linear/phases/claim.md`
/// or `C:\Users\...\.claude\skills\linear\phases\claim.md`.
/// Returns `Some("claim.md")` if it matches a phase file pattern.
fn extract_phase_file(tool_input: &serde_json::Value) -> Option<String> {
    // tool_input for Read is { "file_path": "..." }
    let path = tool_input
        .get("file_path")
        .and_then(|v| v.as_str())?;

    // Normalize separators for cross-platform matching
    let normalized = path.replace('\\', "/");

    // Check if it matches skills/*/phases/*.md
    if let Some(idx) = normalized.find("skills/") {
        let after_skills = &normalized[idx..];
        // Pattern: skills/{name}/phases/{file}.md
        let parts: Vec<&str> = after_skills.split('/').collect();
        if parts.len() >= 4 && parts[2] == "phases" && parts[3].ends_with(".md") {
            return Some(parts[3].to_string());
        }
    }

    None
}

/// Process a phase-gate hook event (PreToolUse)
///
/// This function handles two responsibilities:
/// 1. Track Read() calls on phase files (recording them in state)
/// 2. Gate non-safe tool calls based on workflow phase progress
pub fn process(
    input: &HookInput,
    state: &mut SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
) -> HookOutput {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return HookOutput::allow(),
    };

    // Track ALL tool calls for phase-skip detection
    state.record_tool_call();

    // If this is a Read() call, check if it's reading a phase file
    if tool_name == "Read" {
        if let Some(ref tool_input) = input.tool_input {
            if let Some(phase_file) = extract_phase_file(tool_input) {
                state.record_phase_read(&phase_file);
            }
        }
        // Read calls always pass through (they're safe tools)
        return HookOutput::allow();
    }

    // Delegate to gate evaluation for blocking decisions
    let result = crate::gate::evaluate(state, workflows, input);
    match result {
        crate::gate::GateDecision::Allow => {
            // Additional post-merge skip detection:
            // If review.md is read but qa-handoff.md is not, and we're past
            // the review phase, block non-safe tools
            if let Some(block) = check_post_merge_skip(state, workflows, tool_name) {
                return block;
            }
            HookOutput::allow()
        }
        crate::gate::GateDecision::Block {
            reason,
            next_phase,
            next_phase_file,
        } => {
            let skill = state.active_skill.as_deref().unwrap_or("unknown");
            let completed = state
                .active_workflow()
                .map(|w| w.completed_phases.len())
                .unwrap_or(0);
            let total = workflows
                .get(skill)
                .map(|w| w.phases.iter().filter(|p| p.required).count())
                .unwrap_or(0);

            let message = format_block_box(
                skill,
                &reason,
                &next_phase,
                &next_phase_file,
                completed,
                total,
            );
            HookOutput::block(message)
        }
    }
}

/// Check for post-merge phase skip:
/// If review.md has been read (or review phase completed) but qa-handoff.md
/// has not been read, block non-safe tools. This catches the case where
/// Claude tries to skip QA after code review.
fn check_post_merge_skip(
    state: &SessionState,
    workflows: &HashMap<String, SkillWorkflow>,
    tool_name: &str,
) -> Option<HookOutput> {
    // Only check safe-tool-exempt tools
    let safe_tools = [
        "Read", "Glob", "Grep", "WebSearch", "WebFetch", "Task",
        "AskUserQuestion", "EnterPlanMode", "ExitPlanMode", "TaskCreate",
        "TaskUpdate", "TaskList", "TaskGet", "Skill", "ToolSearch",
    ];
    if safe_tools.contains(&tool_name) {
        return None;
    }

    let skill = state.active_skill.as_ref()?;
    let workflow = workflows.get(skill)?;

    // Check if this workflow has both review and qa-handoff phases
    let has_review = workflow.phases.iter().any(|p| p.id == "review");
    let has_qa = workflow.phases.iter().any(|p| p.id == "qa-handoff");
    if !has_review || !has_qa {
        return None;
    }

    // Check: review.md loaded but qa-handoff.md NOT loaded
    let review_read = state.phases_read.contains(&"review.md".to_string());
    let qa_read = state.phases_read.contains(&"qa-handoff.md".to_string());

    // Also check completed phases (from submit_phase_complete)
    let review_complete = state
        .active_workflow()
        .map(|w| w.is_phase_complete("review"))
        .unwrap_or(false);

    if (review_read || review_complete) && !qa_read {
        let message = format!(
            "\
+============================================================+
|  BLOCKED: Post-Merge Phase Skip Detected                   |
+============================================================+
|  review.md has been loaded but qa-handoff.md has NOT.       |
|                                                            |
|  After code review, you MUST load the QA handoff phase     |
|  before making any further tool calls.                     |
|                                                            |
|  MANDATORY: Read(\"~/.claude/skills/{}/phases/qa-handoff.md\")|
+============================================================+",
            skill
        );
        return Some(HookOutput::block(message));
    }

    None
}

/// Format a visually prominent block message box
fn format_block_box(
    skill: &str,
    reason: &str,
    next_phase: &str,
    next_phase_file: &str,
    completed: usize,
    total: usize,
) -> String {
    format!(
        "\
+============================================================+
|  BLOCKED: Phase Gate Violation                             |
+============================================================+
|  Skill: {skill:<50}|
|  Progress: {completed}/{total} required phases completed              |
|                                                            |
|  Reason: {reason:<49}|
|                                                            |
|  Next required phase: {next_phase:<35}|
|  Read(\"~/.claude/skills/{skill}/phases/{next_phase_file}\")    |
+============================================================+"
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
                    id: "review".to_string(),
                    file: "review.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "Code review".to_string(),
                },
                WorkflowPhase {
                    id: "qa-handoff".to_string(),
                    file: "qa-handoff.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "QA handoff".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_allows_when_no_active_skill() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();
        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_allows_safe_tools() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Glob".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
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
        let output = process(&input, &mut state, &workflows);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("BLOCKED"));
        assert!(output.reason.as_ref().unwrap().contains("claim"));
    }

    #[test]
    fn test_read_on_phase_file_records_it() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "~/.claude/skills/linear/phases/claim.md"
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        // Read should always be allowed
        assert!(output.blocked.is_none());
        // But it should record the phase read
        assert!(state.phases_read.contains(&"claim.md".to_string()));
    }

    #[test]
    fn test_read_on_windows_path_records_phase() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "C:\\Users\\garys\\.claude\\skills\\linear\\phases\\fetch.md"
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        assert!(state.phases_read.contains(&"fetch.md".to_string()));
    }

    #[test]
    fn test_read_on_non_phase_file_ignored() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let input = HookInput {
            tool_name: Some("Read".to_string()),
            tool_input: Some(serde_json::json!({
                "file_path": "~/.claude/skills/linear/SKILL.md"
            })),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert!(output.blocked.is_none());
        assert!(state.phases_read.is_empty());
    }

    #[test]
    fn test_post_merge_skip_blocks() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        // Mark review as read but not qa-handoff
        state.record_phase_read("claim.md");
        state.record_phase_read("fetch.md");
        state.record_phase_read("review.md");

        // Complete claim, fetch, review phases so gate doesn't block on those
        if let Some(wf) = state.workflows.get_mut("linear") {
            wf.advance("claim");
            wf.advance("fetch");
            wf.advance("review");
        }

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        assert_eq!(output.blocked, Some(true));
        assert!(output.reason.as_ref().unwrap().contains("Post-Merge"));
        assert!(output.reason.as_ref().unwrap().contains("qa-handoff.md"));
    }

    #[test]
    fn test_post_merge_skip_allows_when_qa_read() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");
        state.record_phase_read("claim.md");
        state.record_phase_read("fetch.md");
        state.record_phase_read("review.md");
        state.record_phase_read("qa-handoff.md");

        // Complete all phases
        if let Some(wf) = state.workflows.get_mut("linear") {
            wf.advance("claim");
            wf.advance("fetch");
            wf.advance("review");
            wf.advance("qa-handoff");
        }

        let mut workflows = HashMap::new();
        workflows.insert("linear".to_string(), test_workflow());

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        let output = process(&input, &mut state, &workflows);
        // Should be allowed — all phases read
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_tool_call_counter_increments() {
        let mut state = SessionState::new("sess-1");
        let workflows = HashMap::new();

        let input = HookInput {
            tool_name: Some("Bash".to_string()),
            ..Default::default()
        };
        process(&input, &mut state, &workflows);
        process(&input, &mut state, &workflows);
        assert_eq!(state.tool_calls, 2);
    }
}
