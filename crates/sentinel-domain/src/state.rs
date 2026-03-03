//! Session State
//!
//! In-memory state shared across hook engine, MCP server, and dashboard API.
//! This is the single source of truth for a running sentinel daemon.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::evidence::EvidenceCollector;
use crate::proof::ProofChain;
use crate::workflow::WorkflowState;

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
}
