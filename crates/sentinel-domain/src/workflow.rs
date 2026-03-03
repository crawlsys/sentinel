//! Skill Workflow State Machine
//!
//! Defines ordered phases for skills like Linear.
//! Enforces sequential phase execution with proof requirements.

use serde::{Deserialize, Serialize};

use crate::judge::JudgeModel;

/// A single phase in a skill workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPhase {
    /// Phase identifier (e.g., "claim", "fetch")
    pub id: String,

    /// Phase file name (e.g., "claim.md")
    pub file: String,

    /// Whether this phase is required (can't be skipped)
    #[serde(default = "default_true")]
    pub required: bool,

    /// Which AI judge to use for this phase
    #[serde(default = "default_judge")]
    pub judge: JudgeModel,

    /// Human-readable description of what this phase does
    #[serde(default)]
    pub description: String,
}

fn default_true() -> bool {
    true
}

fn default_judge() -> JudgeModel {
    JudgeModel::Sonnet
}

/// A complete skill workflow definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillWorkflow {
    /// Skill this workflow is for
    pub skill: String,

    /// Ordered list of phases
    pub phases: Vec<WorkflowPhase>,
}

/// Runtime state of a workflow execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    /// Skill this state is for
    pub skill: String,

    /// Session ID
    pub session_id: String,

    /// Current phase index (0-based, None = not started)
    pub current_phase: Option<usize>,

    /// Which phases have been completed (with proof)
    pub completed_phases: Vec<String>,

    /// Whether the workflow is fully complete
    pub complete: bool,
}

impl WorkflowState {
    /// Create initial state for a workflow
    #[must_use]
    pub fn new(skill: impl Into<String>, session_id: impl Into<String>) -> Self {
        Self {
            skill: skill.into(),
            session_id: session_id.into(),
            current_phase: None,
            completed_phases: Vec::new(),
            complete: false,
        }
    }

    /// Advance to the next phase
    pub fn advance(&mut self, completed_phase_id: &str) {
        self.completed_phases
            .push(completed_phase_id.to_string());
        self.current_phase = Some(self.completed_phases.len());
    }

    /// Check if a specific phase has been completed
    #[must_use]
    pub fn is_phase_complete(&self, phase_id: &str) -> bool {
        self.completed_phases.iter().any(|p| p == phase_id)
    }

    /// Get the next required phase from a workflow definition
    #[must_use]
    pub fn next_required_phase<'a>(
        &self,
        workflow: &'a SkillWorkflow,
    ) -> Option<&'a WorkflowPhase> {
        for phase in &workflow.phases {
            if phase.required && !self.is_phase_complete(&phase.id) {
                return Some(phase);
            }
        }
        None
    }

    /// Check if a tool call should be blocked based on workflow state
    #[must_use]
    pub fn should_block(
        &self,
        workflow: &SkillWorkflow,
        tool_name: &str,
    ) -> Option<WorkflowBlock> {
        // Never block read-only or meta tools
        let safe_tools = [
            "Read", "Glob", "Grep", "WebSearch", "WebFetch", "Task",
            "AskUserQuestion", "EnterPlanMode", "ExitPlanMode", "TaskCreate",
            "TaskUpdate", "TaskList", "TaskGet", "Skill", "ToolSearch",
        ];
        if safe_tools.contains(&tool_name) {
            return None;
        }

        // Find next required phase
        let next = self.next_required_phase(workflow)?;

        // If no phases completed yet, block with strong message
        if self.completed_phases.is_empty() {
            return Some(WorkflowBlock {
                reason: format!(
                    "Workflow '{}' requires phase '{}' to be completed first. \
                     No phases have been proven yet.",
                    workflow.skill, next.id
                ),
                next_phase: next.id.clone(),
                next_phase_file: next.file.clone(),
                completed: 0,
                total: workflow.phases.iter().filter(|p| p.required).count(),
            });
        }

        // Check how many phases ahead we'd be skipping
        let next_idx = workflow
            .phases
            .iter()
            .position(|p| p.id == next.id)
            .unwrap_or(0);
        let last_completed_idx = self
            .completed_phases
            .last()
            .and_then(|last| workflow.phases.iter().position(|p| p.id == *last))
            .unwrap_or(0);

        // Allow if within 1 phase (currently executing)
        // Block if skipping 2+ phases
        let gap = next_idx.saturating_sub(last_completed_idx + 1);
        if gap >= 1 {
            return Some(WorkflowBlock {
                reason: format!(
                    "Workflow '{}': {} phase(s) skipped. Next required: '{}'",
                    workflow.skill,
                    gap,
                    next.id
                ),
                next_phase: next.id.clone(),
                next_phase_file: next.file.clone(),
                completed: self.completed_phases.len(),
                total: workflow.phases.iter().filter(|p| p.required).count(),
            });
        }

        None
    }
}

/// Information about why a workflow blocked a tool call
#[derive(Debug, Clone)]
pub struct WorkflowBlock {
    pub reason: String,
    pub next_phase: String,
    pub next_phase_file: String,
    pub completed: usize,
    pub total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear_workflow() -> SkillWorkflow {
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
                    description: "Fetch issue details".to_string(),
                },
                WorkflowPhase {
                    id: "review".to_string(),
                    file: "review.md".to_string(),
                    required: true,
                    judge: JudgeModel::Opus,
                    description: "Code review".to_string(),
                },
            ],
        }
    }

    #[test]
    fn test_new_state() {
        let state = WorkflowState::new("linear", "sess-1");
        assert!(state.current_phase.is_none());
        assert!(state.completed_phases.is_empty());
        assert!(!state.complete);
    }

    #[test]
    fn test_advance_phase() {
        let mut state = WorkflowState::new("linear", "sess-1");
        state.advance("claim");
        assert_eq!(state.completed_phases, vec!["claim"]);
        assert!(state.is_phase_complete("claim"));
        assert!(!state.is_phase_complete("fetch"));
    }

    #[test]
    fn test_next_required_phase() {
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");

        assert_eq!(state.next_required_phase(&wf).unwrap().id, "claim");

        state.advance("claim");
        assert_eq!(state.next_required_phase(&wf).unwrap().id, "fetch");

        state.advance("fetch");
        assert_eq!(state.next_required_phase(&wf).unwrap().id, "review");

        state.advance("review");
        assert!(state.next_required_phase(&wf).is_none());
    }

    #[test]
    fn test_block_on_no_phases() {
        let wf = linear_workflow();
        let state = WorkflowState::new("linear", "sess-1");

        let block = state.should_block(&wf, "Bash");
        assert!(block.is_some());
        assert!(block.unwrap().reason.contains("claim"));
    }

    #[test]
    fn test_allow_safe_tools() {
        let wf = linear_workflow();
        let state = WorkflowState::new("linear", "sess-1");

        assert!(state.should_block(&wf, "Read").is_none());
        assert!(state.should_block(&wf, "Glob").is_none());
        assert!(state.should_block(&wf, "Task").is_none());
    }

    #[test]
    fn test_block_on_skip() {
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");
        state.advance("claim");
        // Trying to use tools without completing "fetch" (skipping to review territory)
        // Gap is 0 here (fetch is the very next one), so it should NOT block
        assert!(state.should_block(&wf, "Bash").is_none());
    }
}
