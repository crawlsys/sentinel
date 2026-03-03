//! Session State
//!
//! In-memory state shared across hook engine, MCP server, and dashboard API.
//! This is the single source of truth for a running sentinel daemon.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::evidence::EvidenceCollector;
use crate::proof::ProofChain;
use crate::workflow::{SkillWorkflow, WorkflowState};

/// Full session state — shared across all sentinel modes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// Session ID (from Claude Code)
    pub session_id: String,

    /// When this session started
    pub started_at: DateTime<Utc>,

    /// Active skill (detected by router)
    pub active_skill: Option<String>,

    /// Workflow states per skill
    pub workflows: HashMap<String, WorkflowState>,

    /// Proof chains per skill
    pub proof_chains: HashMap<String, ProofChain>,

    /// Hook execution counts
    pub hook_stats: HookStats,

    /// Whether the session is still active
    pub active: bool,

    /// Phase files that have been Read() by Claude (e.g., "claim.md", "fetch.md")
    #[serde(default)]
    pub phases_read: Vec<String>,

    /// Total tool calls in this session (for phase-skip detection)
    #[serde(default)]
    pub tool_calls: u32,
}

/// Aggregated hook execution statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookStats {
    /// Total hook invocations
    pub total_invocations: u64,

    /// Total tool calls blocked
    pub total_blocked: u64,

    /// Per-hook execution counts
    pub per_hook: HashMap<String, u64>,

    /// Per-hook total execution time in ms
    pub per_hook_time_ms: HashMap<String, u64>,
}

impl SessionState {
    /// Create a new session state
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            started_at: Utc::now(),
            active_skill: None,
            workflows: HashMap::new(),
            proof_chains: HashMap::new(),
            hook_stats: HookStats::default(),
            active: true,
            phases_read: Vec::new(),
            tool_calls: 0,
        }
    }

    /// Set the active skill (from skill router)
    pub fn set_active_skill(&mut self, skill: impl Into<String>) {
        let skill = skill.into();
        // Initialize workflow state if not exists
        if !self.workflows.contains_key(&skill) {
            self.workflows.insert(
                skill.clone(),
                WorkflowState::new(&skill, &self.session_id),
            );
        }
        // Initialize proof chain if not exists
        if !self.proof_chains.contains_key(&skill) {
            self.proof_chains.insert(
                skill.clone(),
                ProofChain::new(&skill, &self.session_id),
            );
        }
        self.active_skill = Some(skill);
    }

    /// Get the workflow state for the active skill
    #[must_use]
    pub fn active_workflow(&self) -> Option<&WorkflowState> {
        self.active_skill
            .as_ref()
            .and_then(|s| self.workflows.get(s))
    }

    /// Get the proof chain for the active skill
    #[must_use]
    pub fn active_proof_chain(&self) -> Option<&ProofChain> {
        self.active_skill
            .as_ref()
            .and_then(|s| self.proof_chains.get(s))
    }

    /// Get mutable workflow state for the active skill
    pub fn active_workflow_mut(&mut self) -> Option<&mut WorkflowState> {
        self.active_skill
            .clone()
            .and_then(move |s| self.workflows.get_mut(&s))
    }

    /// Get mutable proof chain for the active skill
    pub fn active_proof_chain_mut(&mut self) -> Option<&mut ProofChain> {
        self.active_skill
            .clone()
            .and_then(move |s| self.proof_chains.get_mut(&s))
    }

    /// Record a hook invocation
    pub fn record_hook_invocation(&mut self, hook_id: &str, duration_ms: u64) {
        self.hook_stats.total_invocations += 1;
        *self.hook_stats.per_hook.entry(hook_id.to_string()).or_insert(0) += 1;
        *self
            .hook_stats
            .per_hook_time_ms
            .entry(hook_id.to_string())
            .or_insert(0) += duration_ms;
    }

    /// Record a blocked tool call
    pub fn record_blocked(&mut self) {
        self.hook_stats.total_blocked += 1;
    }

    /// Record that a phase file has been Read() by Claude.
    /// Only adds if not already present (idempotent).
    pub fn record_phase_read(&mut self, phase_file: &str) {
        let file = phase_file.to_string();
        if !self.phases_read.contains(&file) {
            self.phases_read.push(file);
        }
    }

    /// Number of phase files that have been read
    #[must_use]
    pub fn phases_read_count(&self) -> usize {
        self.phases_read.len()
    }

    /// Returns the file name of the next required phase that hasn't been read yet.
    /// Uses the workflow definition to determine phase ordering.
    #[must_use]
    pub fn next_required_phase_file(&self, workflow: &SkillWorkflow) -> Option<String> {
        for phase in &workflow.phases {
            if phase.required && !self.phases_read.contains(&phase.file) {
                return Some(phase.file.clone());
            }
        }
        None
    }

    /// Record a tool call (increments counter)
    pub fn record_tool_call(&mut self) {
        self.tool_calls += 1;
    }
}

/// Evidence collection state — NOT serialized (transient, per-phase)
#[derive(Debug)]
pub struct PhaseCollectionState {
    /// Phase being collected for
    pub phase_id: String,

    /// Skill this phase belongs to
    pub skill: String,

    /// Evidence collector
    pub collector: EvidenceCollector,

    /// When collection started
    pub started_at: DateTime<Utc>,
}

impl PhaseCollectionState {
    #[must_use]
    pub fn new(phase_id: impl Into<String>, skill: impl Into<String>) -> Self {
        Self {
            phase_id: phase_id.into(),
            skill: skill.into(),
            collector: EvidenceCollector::new(),
            started_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_state() {
        let state = SessionState::new("sess-1");
        assert_eq!(state.session_id, "sess-1");
        assert!(state.active);
        assert!(state.active_skill.is_none());
        assert!(state.workflows.is_empty());
    }

    #[test]
    fn test_set_active_skill() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");

        assert_eq!(state.active_skill.as_deref(), Some("linear"));
        assert!(state.workflows.contains_key("linear"));
        assert!(state.proof_chains.contains_key("linear"));
    }

    #[test]
    fn test_active_workflow() {
        let mut state = SessionState::new("sess-1");
        assert!(state.active_workflow().is_none());

        state.set_active_skill("linear");
        assert!(state.active_workflow().is_some());
        assert_eq!(state.active_workflow().unwrap().skill, "linear");
    }

    #[test]
    fn test_hook_stats() {
        let mut state = SessionState::new("sess-1");
        state.record_hook_invocation("skill-router", 15);
        state.record_hook_invocation("skill-router", 12);
        state.record_hook_invocation("phase-gate", 3);
        state.record_blocked();

        assert_eq!(state.hook_stats.total_invocations, 3);
        assert_eq!(state.hook_stats.total_blocked, 1);
        assert_eq!(state.hook_stats.per_hook["skill-router"], 2);
        assert_eq!(state.hook_stats.per_hook_time_ms["skill-router"], 27);
    }

    #[test]
    fn test_set_active_skill_idempotent() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");

        // Advance workflow
        state.active_workflow_mut().unwrap().advance("claim");

        // Setting same skill again should NOT reset the workflow
        state.set_active_skill("linear");
        assert!(state.active_workflow().unwrap().is_phase_complete("claim"));
    }

    #[test]
    fn test_record_phase_read() {
        let mut state = SessionState::new("sess-1");
        assert_eq!(state.phases_read_count(), 0);

        state.record_phase_read("claim.md");
        assert_eq!(state.phases_read_count(), 1);
        assert!(state.phases_read.contains(&"claim.md".to_string()));

        // Idempotent — recording same file twice doesn't duplicate
        state.record_phase_read("claim.md");
        assert_eq!(state.phases_read_count(), 1);

        state.record_phase_read("fetch.md");
        assert_eq!(state.phases_read_count(), 2);
    }

    #[test]
    fn test_next_required_phase_file() {
        use crate::judge::JudgeModel;
        use crate::workflow::WorkflowPhase;

        let workflow = SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![
                WorkflowPhase {
                    id: "claim".to_string(),
                    file: "claim.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: String::new(),
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: String::new(),
                },
                WorkflowPhase {
                    id: "cleanup".to_string(),
                    file: "cleanup.md".to_string(),
                    required: false,
                    judge: JudgeModel::Sonnet,
                    description: String::new(),
                },
            ],
        };

        let mut state = SessionState::new("sess-1");

        // First required phase is claim.md
        assert_eq!(
            state.next_required_phase_file(&workflow),
            Some("claim.md".to_string())
        );

        // After reading claim.md, next is fetch.md
        state.record_phase_read("claim.md");
        assert_eq!(
            state.next_required_phase_file(&workflow),
            Some("fetch.md".to_string())
        );

        // After reading fetch.md, no more required phases
        state.record_phase_read("fetch.md");
        assert_eq!(state.next_required_phase_file(&workflow), None);
    }

    #[test]
    fn test_tool_call_counter() {
        let mut state = SessionState::new("sess-1");
        assert_eq!(state.tool_calls, 0);

        state.record_tool_call();
        assert_eq!(state.tool_calls, 1);

        state.record_tool_call();
        state.record_tool_call();
        assert_eq!(state.tool_calls, 3);
    }

    #[test]
    fn test_new_state_has_empty_phases() {
        let state = SessionState::new("sess-1");
        assert!(state.phases_read.is_empty());
        assert_eq!(state.tool_calls, 0);
    }
}
