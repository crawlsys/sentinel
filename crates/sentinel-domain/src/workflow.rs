//! Skill Workflow State Machine
//!
//! Defines ordered phases for skills like Linear.
//! Enforces sequential phase execution with proof requirements.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::judge::JudgeModel;

// ============================================================================
// Step definitions (loaded from config/steps/<skill>.toml)
// ============================================================================

/// A trackable step within a phase
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Step identifier (e.g., "-0.1", "3.L2.3")
    pub id: String,

    /// Human-readable description
    pub description: String,

    /// Whether this step is a blocker/gate (failure stops progress)
    #[serde(default)]
    pub blocker: bool,
}

/// Steps for a single phase
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseSteps {
    /// Phase this belongs to (e.g., "claim", "review")
    #[serde(rename = "id")]
    pub phase_id: String,

    /// Ordered list of steps
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
}

/// All step definitions for a skill
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSteps {
    /// Skill name — skipped during serde because it is set by
    /// `config::load_skill_steps()` from the filename, not from TOML content.
    /// This struct is never round-tripped through JSON.
    #[serde(skip)]
    pub skill: String,

    /// Steps per phase
    pub phases: Vec<PhaseSteps>,
}

impl SkillSteps {
    /// Get steps for a specific phase
    #[must_use]
    pub fn phase_steps(&self, phase_id: &str) -> Option<&PhaseSteps> {
        self.phases.iter().find(|p| p.phase_id == phase_id)
    }

    /// Total number of steps across all phases
    #[must_use]
    pub fn total_steps(&self) -> usize {
        self.phases.iter().map(|p| p.steps.len()).sum()
    }
}

// ============================================================================
// Phase definitions (loaded from config/workflows.toml)
// ============================================================================

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

    /// Tool name prefixes to block when this workflow is active.
    /// E.g., `["mcp__cdp__"]` blocks all CDP tools when steel workflow is active.
    /// This prevents using equivalent MCP servers to bypass phase enforcement.
    #[serde(default)]
    pub blocked_tool_prefixes: Vec<String>,

    /// Bash command patterns (regex strings) to block when this workflow is active.
    /// E.g., `["steel-mcp", "chrome.*--remote-debugging"]` blocks CLI escape attempts.
    #[serde(default)]
    pub blocked_bash_patterns: Vec<String>,

    /// Bash command allowlist (regex strings). When non-empty, ONLY commands matching
    /// at least one pattern are permitted. All other Bash commands are blocked.
    /// This is the nuclear option — defeats all obfuscation (eval, base64, $'\x',
    /// variable concatenation) because anything not explicitly allowed is denied.
    /// E.g., `["^git ", "^npm ", "^node ", "^cargo "]` allows only those prefixes.
    #[serde(default)]
    pub bash_allowlist: Vec<String>,
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

    /// Step-level tracking within phases
    #[serde(default)]
    pub step_states: Vec<StepState>,

    /// Currently active step ID
    #[serde(default)]
    pub current_step: Option<String>,
}

/// Runtime state of a single step within a phase
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepState {
    /// Step identifier (e.g., "0.1", "3.L2.3")
    pub step_id: String,

    /// Phase this step belongs to
    pub phase_id: String,

    /// Current status
    pub status: StepStatus,

    /// When this step started
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,

    /// When this step completed
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,

    /// Brief summary of what happened
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Status of a workflow step
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Skipped,
    Blocked,
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
            step_states: Vec::new(),
            current_step: None,
        }
    }

    /// Advance to the next phase (idempotent — no-op if already completed).
    ///
    /// **Sequential enforcement**: Only advances if all prior required phases
    /// in the workflow are already complete. This prevents out-of-order phase
    /// reads from unlocking tools (Attack #21). Returns true if advanced.
    pub fn advance_sequential(
        &mut self,
        completed_phase_id: &str,
        workflow: &SkillWorkflow,
    ) -> bool {
        if self.is_phase_complete(completed_phase_id) {
            return false; // Already done — idempotent
        }

        // Find this phase's position in the workflow
        let target_idx = match workflow
            .phases
            .iter()
            .position(|p| p.id == completed_phase_id)
        {
            Some(idx) => idx,
            None => return false, // Unknown phase
        };

        // Check that ALL prior required phases are completed
        for phase in &workflow.phases[..target_idx] {
            if phase.required && !self.is_phase_complete(&phase.id) {
                // Prior required phase not complete — refuse to advance
                eprintln!(
                    "[sentinel] Sequential enforcement: refusing to advance '{}' because \
                     prior required phase '{}' is not yet complete.",
                    completed_phase_id, phase.id
                );
                return false;
            }
        }

        self.completed_phases.push(completed_phase_id.to_string());
        self.current_phase = Some(self.completed_phases.len());

        // **Attack #139 fix**: Set the `complete` flag when all required phases
        // are done. Without this, the flag was always false — meaning code that
        // checks `workflow_state.complete` (e.g., daemon API, proof chain display)
        // would never report a workflow as finished.
        if self.next_required_phase(workflow).is_none() {
            self.complete = true;
        }

        true
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
    pub fn should_block(&self, workflow: &SkillWorkflow, tool_name: &str) -> Option<WorkflowBlock> {
        // Phase-exempt tools: discovery, metadata, and plan-approval calls that
        // don't count as workflow phase execution. Calling one of these does NOT
        // count as "doing the work" — no code runs, no files are written, no
        // subprocesses spawn. Tools that CAN execute arbitrary code (Bash, Edit,
        // Write, Task, Agent, Skill, SendMessage, MCP tools) are deliberately
        // NOT in this list — they get gated by phase progress.
        //
        // **Attack #69**: Task/Agent are blocked — they spawn subcontexts that
        //   may not inherit the same sentinel hooks.
        // **Attack #107**: Skill is blocked — nested workflow activations can
        //   confuse the state machine.
        // **Attack #108**: SendMessage is blocked — can leak phase content to
        //   teammate contexts without sentinel enforcement.
        // **Attack #99**: ToolSearch is exempt — it only fetches JSON schemas,
        //   no execution.
        // **Attack #101**: NotebookEdit is NOT exempt — can modify code cells.
        //
        // Per-tool rationale:
        //   Read/Glob/Grep — filesystem discovery, read-only
        //   WebSearch/WebFetch — external read-only
        //   AskUserQuestion — prompts the user, no code execution
        //   ExitPlanMode — writes a plan file + requests approval, no side-effectful work
        //     (note: there is no `EnterPlanMode` tool — plan mode is entered via
        //     Shift+Tab, CLAUDE_CODE_PLAN_MODE_REQUIRED env var, Agent with
        //     mode:"plan", or agent-YAML permissionMode:"plan"; verified against
        //     claude-code-2.1.88 package/sdk-tools.d.ts)
        //   TodoWrite — core Claude Code todo list, metadata-only
        //   TaskCreate/TaskUpdate/TaskList/TaskGet/TaskOutput/TaskStop — agent-team
        //     task management, metadata-only
        //   ToolSearch — schema fetcher, read-only
        let phase_exempt_tools = [
            "Read",
            "Glob",
            "Grep",
            "WebSearch",
            "WebFetch",
            "AskUserQuestion",
            "ExitPlanMode",
            "TodoWrite",
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "TaskGet",
            "TaskOutput",
            "TaskStop",
            "ToolSearch",
        ];
        if phase_exempt_tools.contains(&tool_name) {
            return None;
        }

        // ── Blocked tool prefix check ─────────────────────────────────
        // Block equivalent MCP tools regardless of phase progress.
        // E.g., mcp__cdp__* is always blocked when steel workflow is active.
        if !workflow.blocked_tool_prefixes.is_empty() {
            for prefix in &workflow.blocked_tool_prefixes {
                if tool_name.starts_with(prefix.as_str()) {
                    let next = self.next_required_phase(workflow);
                    let next_phase = next.map(|n| n.id.clone()).unwrap_or_default();
                    let next_file = next.map(|n| n.file.clone()).unwrap_or_default();
                    return Some(WorkflowBlock {
                        reason: format!(
                            "Workflow '{}': tool '{}' is blocked (matches blocked prefix '{}').\n\
                             Use the workflow's native tools instead of equivalent alternatives.",
                            workflow.skill, tool_name, prefix
                        ),
                        next_phase,
                        next_phase_file: next_file,
                        completed: self.completed_phases.len(),
                        total: workflow.phases.iter().filter(|p| p.required).count(),
                    });
                }
            }
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
                    workflow.skill, gap, next.id
                ),
                next_phase: next.id.clone(),
                next_phase_file: next.file.clone(),
                completed: self.completed_phases.len(),
                total: workflow.phases.iter().filter(|p| p.required).count(),
            });
        }

        None
    }

    // ========================================================================
    // Step tracking methods
    // ========================================================================

    /// Update a step's status. Creates the step state if it doesn't exist.
    pub fn update_step(
        &mut self,
        phase_id: &str,
        step_id: &str,
        status: StepStatus,
        summary: Option<String>,
    ) {
        let now = Utc::now();

        if let Some(existing) = self
            .step_states
            .iter_mut()
            .find(|s| s.step_id == step_id && s.phase_id == phase_id)
        {
            existing.status = status.clone();
            existing.summary = summary;
            if status == StepStatus::Completed || status == StepStatus::Skipped {
                existing.completed_at = Some(now);
            }
        } else {
            self.step_states.push(StepState {
                step_id: step_id.to_string(),
                phase_id: phase_id.to_string(),
                status: status.clone(),
                started_at: Some(now),
                completed_at: if status == StepStatus::Completed || status == StepStatus::Skipped {
                    Some(now)
                } else {
                    None
                },
                summary,
            });
        }

        if status == StepStatus::InProgress {
            self.current_step = Some(step_id.to_string());
        }
    }

    /// Get all step states for a specific phase
    #[must_use]
    pub fn phase_step_states(&self, phase_id: &str) -> Vec<&StepState> {
        self.step_states
            .iter()
            .filter(|s| s.phase_id == phase_id)
            .collect()
    }

    /// Count completed steps for a specific phase
    #[must_use]
    pub fn phase_steps_completed(&self, phase_id: &str) -> usize {
        self.step_states
            .iter()
            .filter(|s| {
                s.phase_id == phase_id
                    && (s.status == StepStatus::Completed || s.status == StepStatus::Skipped)
            })
            .count()
    }

    /// Count total completed steps across all phases
    #[must_use]
    pub fn total_steps_completed(&self) -> usize {
        self.step_states
            .iter()
            .filter(|s| s.status == StepStatus::Completed || s.status == StepStatus::Skipped)
            .count()
    }

    /// Get completed step IDs for a specific phase (for evidence)
    #[must_use]
    pub fn completed_step_ids(&self, phase_id: &str) -> Vec<String> {
        self.step_states
            .iter()
            .filter(|s| s.phase_id == phase_id && s.status == StepStatus::Completed)
            .map(|s| s.step_id.clone())
            .collect()
    }

    /// Get skipped step IDs for a specific phase (for evidence)
    #[must_use]
    pub fn skipped_step_ids(&self, phase_id: &str) -> Vec<String> {
        self.step_states
            .iter()
            .filter(|s| s.phase_id == phase_id && s.status == StepStatus::Skipped)
            .map(|s| s.step_id.clone())
            .collect()
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
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
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
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");
        state.advance_sequential("claim", &wf);
        assert_eq!(state.completed_phases, vec!["claim"]);
        assert!(state.is_phase_complete("claim"));
        assert!(!state.is_phase_complete("fetch"));
    }

    #[test]
    fn test_next_required_phase() {
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");

        assert_eq!(state.next_required_phase(&wf).unwrap().id, "claim");

        state.advance_sequential("claim", &wf);
        assert_eq!(state.next_required_phase(&wf).unwrap().id, "fetch");

        state.advance_sequential("fetch", &wf);
        assert_eq!(state.next_required_phase(&wf).unwrap().id, "review");

        state.advance_sequential("review", &wf);
        assert!(state.next_required_phase(&wf).is_none());
    }

    #[test]
    fn test_advance_sequential_sets_complete() {
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");

        // Not complete yet
        assert!(!state.complete);

        // Advance through all required phases
        assert!(state.advance_sequential("claim", &wf));
        assert!(!state.complete);
        assert!(state.advance_sequential("fetch", &wf));
        assert!(!state.complete);
        assert!(state.advance_sequential("review", &wf));

        // Now all required phases done — complete should be true
        assert!(state.complete);
        assert!(state.next_required_phase(&wf).is_none());
    }

    #[test]
    fn test_advance_sequential_rejects_out_of_order() {
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");

        // Trying to advance "fetch" before "claim" — should fail
        assert!(!state.advance_sequential("fetch", &wf));
        assert!(!state.is_phase_complete("fetch"));
        assert!(!state.complete);
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
    fn test_phase_exempt_tools_not_blocked() {
        let wf = linear_workflow();
        let state = WorkflowState::new("linear", "sess-1");

        // Discovery and metadata tools — no workflow execution, never gated.
        assert!(state.should_block(&wf, "Read").is_none());
        assert!(state.should_block(&wf, "Glob").is_none());
        assert!(state.should_block(&wf, "Grep").is_none());
        assert!(state.should_block(&wf, "WebSearch").is_none());
        assert!(state.should_block(&wf, "AskUserQuestion").is_none());
        assert!(state.should_block(&wf, "ExitPlanMode").is_none());
        assert!(state.should_block(&wf, "TodoWrite").is_none());
        assert!(state.should_block(&wf, "ToolSearch").is_none());

        // Code-executing and context-spawning tools — always gated.
        // Attack #69: Task/Agent spawn subcontexts that may skip sentinel.
        assert!(state.should_block(&wf, "Task").is_some());
        assert!(state.should_block(&wf, "Agent").is_some());
        // Attacks #107/#108: Skill/SendMessage can nest workflows or leak phase content.
        assert!(state.should_block(&wf, "Skill").is_some());
        assert!(state.should_block(&wf, "SendMessage").is_some());

        // Fake tool names MUST NOT sneak back in. `EnterPlanMode` does not
        // exist in claude-code-2.1.88 — this regression test ensures the
        // exempt list doesn't grow it back.
        assert!(state.should_block(&wf, "EnterPlanMode").is_some(),
            "EnterPlanMode is not a real tool — must be gated (treated as unknown)");
    }

    #[test]
    fn test_block_on_skip() {
        let wf = linear_workflow();
        let mut state = WorkflowState::new("linear", "sess-1");
        state.advance_sequential("claim", &wf);
        // Trying to use tools without completing "fetch" (skipping to review territory)
        // Gap is 0 here (fetch is the very next one), so it should NOT block
        assert!(state.should_block(&wf, "Bash").is_none());
    }
}
