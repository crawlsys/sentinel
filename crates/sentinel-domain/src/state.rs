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

    /// Phase files that have been `Read()` by Claude, keyed by skill name.
    /// E.g., `{"linear": ["claim.md", "fetch.md"], "steel": ["claim.md"]}`.
    ///
    /// **Attack #50**: Was a global `Vec<String>` — reading skill A's `review.md`
    /// would satisfy checks for skill B's `review.md`. Now per-skill.
    #[serde(default)]
    pub phases_read: HashMap<String, Vec<String>>,

    /// Total tool calls in this session (for phase-skip detection)
    /// **Attack #152 fix**: Changed from u32 to u64 to match `HookStats.total_invocations`.
    /// u32 wraps at ~4.3B, silently corrupting audit data in long-running sessions.
    #[serde(default)]
    pub tool_calls: u64,

    /// Failed submission attempts per phase key ("`skill:phase_id`")
    /// Used for resubmission rate limiting
    #[serde(default)]
    pub failed_submissions: HashMap<String, SubmissionAttempts>,

    /// SHA-256 hashes of phase file content, keyed by canonical path.
    /// Set on first trusted `Read()` of a phase file. Subsequent reads with
    /// different content indicate mid-session file tampering.
    #[serde(default)]
    pub phase_file_hashes: HashMap<String, String>,

    /// Monotonic state generation counter. Incremented on every `save()`.
    /// **Attack #81 fix**: Detects state regression from file deletion/replacement.
    /// If loaded state has a lower generation than the in-memory counter,
    /// someone deleted and recreated the state file mid-session.
    #[serde(default)]
    pub state_generation: u64,

    /// Glass break emergency override — temporarily suspends workflow enforcement.
    /// When active (and not expired), phase gate allows all tools through.
    #[serde(default)]
    pub glass_break: Option<GlassBreak>,
}

/// Glass break emergency override state.
///
/// Initiated via `sentinel break --reason "..."` from an interactive terminal.
/// Suspends all workflow restrictions (bash allowlist, phase gate, blocked tool prefixes)
/// for a limited duration. All tool calls during the break are recorded for audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlassBreak {
    /// Human-readable reason for the break
    pub reason: String,

    /// When the break was initiated
    pub started_at: DateTime<Utc>,

    /// When the break expires (auto-reengage)
    pub expires_at: DateTime<Utc>,

    /// Duration in minutes (for display/logging)
    pub duration_minutes: u32,

    /// Optional: specific workflow being broken (None = all workflows)
    pub workflow: Option<String>,

    /// The 6-digit challenge code that was confirmed
    pub challenge_code: String,

    /// All tool calls made during the break (audit trail)
    pub tools_used: Vec<BreakToolUse>,
}

/// A tool call made during an active glass break (audit record).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakToolUse {
    /// Tool name (e.g., "Bash", "Edit", "Write")
    pub tool: String,

    /// Detail — command for Bash, `file_path` for Edit/Write
    pub detail: String,

    /// ISO 8601 timestamp
    pub ts: String,
}

/// Tracks failed submission attempts for rate limiting
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubmissionAttempts {
    /// Number of consecutive failures
    pub count: u32,
    /// Timestamp of last failure
    pub last_failure: Option<DateTime<Utc>>,
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
            phases_read: HashMap::new(),
            tool_calls: 0,
            failed_submissions: HashMap::new(),
            phase_file_hashes: HashMap::new(),
            state_generation: 0,
            glass_break: None,
        }
    }

    /// **Attack #169 fix**: Maximum distinct skills per session.
    /// Prevents unbounded `HashMap` growth from skill router manipulation.
    const MAX_SKILLS_PER_SESSION: usize = 100;

    /// Set the active skill (from skill router)
    pub fn set_active_skill(&mut self, skill: impl Into<String>) {
        let skill = skill.into();
        // Initialize workflow state if not exists
        if !self.workflows.contains_key(&skill) {
            // **Attack #169 fix**: Reject new skills beyond the cap.
            if self.workflows.len() >= Self::MAX_SKILLS_PER_SESSION {
                eprintln!(
                    "[sentinel] WARNING: Session '{}' hit max skill limit ({}). \
                     Ignoring new skill '{}'. This may indicate skill router manipulation.",
                    self.session_id,
                    Self::MAX_SKILLS_PER_SESSION,
                    skill,
                );
                // Still set active_skill so routing works, but don't allocate new state
                self.active_skill = Some(skill);
                return;
            }
            self.workflows
                .insert(skill.clone(), WorkflowState::new(&skill, &self.session_id));
        }
        // Initialize proof chain if not exists
        if !self.proof_chains.contains_key(&skill) {
            self.proof_chains
                .insert(skill.clone(), ProofChain::new(&skill, &self.session_id));
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
        *self
            .hook_stats
            .per_hook
            .entry(hook_id.to_string())
            .or_insert(0) += 1;
        *self
            .hook_stats
            .per_hook_time_ms
            .entry(hook_id.to_string())
            .or_insert(0) += duration_ms;
    }

    /// Record a blocked tool call
    pub const fn record_blocked(&mut self) {
        self.hook_stats.total_blocked += 1;
    }

    /// Record that a phase file has been `Read()` by Claude for a specific skill.
    /// Only adds if not already present (idempotent).
    pub fn record_phase_read(&mut self, skill: &str, phase_file: &str) {
        let files = self.phases_read.entry(skill.to_string()).or_default();
        let file = phase_file.to_string();
        if !files.contains(&file) {
            files.push(file);
        }
    }

    /// Number of phase files that have been read across all skills
    #[must_use]
    pub fn phases_read_count(&self) -> usize {
        self.phases_read.values().map(std::vec::Vec::len).sum()
    }

    /// Check if a specific phase file has been read for a given skill
    #[must_use]
    pub fn has_phase_been_read(&self, skill: &str, phase_file: &str) -> bool {
        self.phases_read
            .get(skill)
            .is_some_and(|files| files.contains(&phase_file.to_string()))
    }

    /// Returns the file name of the next required phase that hasn't been read yet.
    /// Uses the workflow definition to determine phase ordering.
    #[must_use]
    pub fn next_required_phase_file(&self, workflow: &SkillWorkflow) -> Option<String> {
        let skill_files = self.phases_read.get(&workflow.skill);
        for phase in &workflow.phases {
            if phase.required {
                let read = skill_files.is_some_and(|files| files.contains(&phase.file));
                if !read {
                    return Some(phase.file.clone());
                }
            }
        }
        None
    }

    /// Check if a glass break is currently active (not expired).
    #[must_use]
    pub fn is_break_active(&self) -> bool {
        self.glass_break
            .as_ref()
            .is_some_and(|gb| Utc::now() < gb.expires_at)
    }

    /// Clear the glass break if it has expired. Preserves break data for
    /// audit purposes until explicitly cleared — the `is_break_active()`
    /// check handles expiry semantically.
    pub fn clear_expired_break(&mut self) {
        if let Some(ref gb) = self.glass_break {
            if Utc::now() >= gb.expires_at {
                self.glass_break = None;
            }
        }
    }

    /// Record a tool call (increments counter)
    pub const fn record_tool_call(&mut self) {
        self.tool_calls += 1;
    }

    // ─── Submission failure tracking (rate limiting) ──────────────────────
    //
    // Replaces three former direct mutations of `failed_submissions` from
    // `proof_engine`. Keeping the mutation surface inside the aggregate lets
    // future invariants (e.g. cap the map size, expire stale entries) live
    // in one place.

    /// If a recent failure for `phase_key` is still within the cooldown
    /// window, return the remaining seconds. `None` means submission is
    /// allowed.
    ///
    /// Cooldown formula: `base_cooldown_secs` when `count < max_rapid`,
    /// otherwise `base_cooldown_secs * count` (linear backoff). The two
    /// thresholds are passed in so the caller can use the project-wide
    /// constants (`PROOF_RESUBMIT_COOLDOWN_SECS`, `PROOF_MAX_RAPID_FAILURES`)
    /// or override for tests.
    #[must_use]
    pub fn submission_cooldown_remaining(
        &self,
        phase_key: &str,
        max_rapid: u32,
        base_cooldown_secs: i64,
    ) -> Option<i64> {
        let attempts = self.failed_submissions.get(phase_key)?;
        let last = attempts.last_failure?;
        let elapsed = (Utc::now() - last).num_seconds();
        let cooldown = if attempts.count >= max_rapid {
            base_cooldown_secs * i64::from(attempts.count)
        } else {
            base_cooldown_secs
        };
        if elapsed < cooldown {
            Some(cooldown - elapsed)
        } else {
            None
        }
    }

    /// Borrow the submission-attempts record for the given phase key.
    /// Used by the proof engine to surface the failure count in
    /// human-readable error messages.
    #[must_use]
    pub fn submission_attempts(&self, phase_key: &str) -> Option<&SubmissionAttempts> {
        self.failed_submissions.get(phase_key)
    }

    /// Record a failed submission for `phase_key`. Bumps the attempt count
    /// and stamps `last_failure` to now. Creates the entry if missing.
    pub fn record_submission_failure(&mut self, phase_key: impl Into<String>) {
        let entry = self.failed_submissions.entry(phase_key.into()).or_default();
        entry.count += 1;
        entry.last_failure = Some(Utc::now());
    }

    /// Clear submission-failure tracking for `phase_key` (called after a
    /// successful submission so the next attempt is unrestricted).
    pub fn clear_submission_failure(&mut self, phase_key: &str) {
        self.failed_submissions.remove(phase_key);
    }

    /// Record the SHA-256 hash of a phase file's content on first `Read()`.
    /// Returns `Ok(())` if this is the first read or the hash matches.
    /// Returns `Err(reason)` if the content has changed (tampering detected).
    /// **Attack #106 fix**: Normalize hash keys to lowercase on Windows.
    /// Windows paths are case-insensitive, so `skills/Linear/phases/Claim.md`
    /// and `skills/linear/phases/claim.md` reference the same file. Without
    /// normalization, an attacker could read a phase file with altered casing
    /// to bypass the tamper-detection check (different key → no prior hash).
    pub fn record_phase_file_hash(
        &mut self,
        canonical_path: &str,
        content_hash: &str,
    ) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        let canonical_path = &canonical_path.to_lowercase();
        match self.phase_file_hashes.get(canonical_path) {
            Some(existing) if existing != content_hash => Err(format!(
                "Phase file content changed mid-session: '{canonical_path}'. \
                     Original hash: {existing}, new hash: {content_hash}. \
                     This indicates file tampering."
            )),
            Some(_) => Ok(()), // Same hash — no change
            None => {
                self.phase_file_hashes
                    .insert(canonical_path.to_string(), content_hash.to_string());
                Ok(())
            }
        }
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

        // Advance workflow — need a workflow definition for advance_sequential
        let wf = crate::workflow::SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![crate::workflow::WorkflowPhase {
                id: "claim".to_string(),
                file: "claim.md".to_string(),
                required: true,
                judge: crate::judge::JudgeModel::Sonnet,
                description: "Claim".to_string(),
            }],
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        };
        state
            .active_workflow_mut()
            .unwrap()
            .advance_sequential("claim", &wf);

        // Setting same skill again should NOT reset the workflow
        state.set_active_skill("linear");
        assert!(state.active_workflow().unwrap().is_phase_complete("claim"));
    }

    #[test]
    fn test_record_phase_read() {
        let mut state = SessionState::new("sess-1");
        assert_eq!(state.phases_read_count(), 0);

        state.record_phase_read("linear", "claim.md");
        assert_eq!(state.phases_read_count(), 1);
        assert!(state.has_phase_been_read("linear", "claim.md"));
        assert!(!state.has_phase_been_read("steel", "claim.md")); // per-skill isolation

        // Idempotent — recording same file twice doesn't duplicate
        state.record_phase_read("linear", "claim.md");
        assert_eq!(state.phases_read_count(), 1);

        state.record_phase_read("linear", "fetch.md");
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
            blocked_tool_prefixes: Vec::new(),
            blocked_bash_patterns: Vec::new(),
            bash_allowlist: Vec::new(),
        };

        let mut state = SessionState::new("sess-1");

        // First required phase is claim.md
        assert_eq!(
            state.next_required_phase_file(&workflow),
            Some("claim.md".to_string())
        );

        // After reading claim.md, next is fetch.md
        state.record_phase_read("linear", "claim.md");
        assert_eq!(
            state.next_required_phase_file(&workflow),
            Some("fetch.md".to_string())
        );

        // After reading fetch.md, no more required phases
        state.record_phase_read("linear", "fetch.md");
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

    #[test]
    fn test_max_skills_per_session() {
        let mut state = SessionState::new("sess-1");
        // Fill to the cap
        for i in 0..SessionState::MAX_SKILLS_PER_SESSION {
            state.set_active_skill(format!("skill_{i}"));
        }
        assert_eq!(state.workflows.len(), SessionState::MAX_SKILLS_PER_SESSION);

        // One more should NOT create a new workflow entry
        state.set_active_skill("overflow_skill");
        assert_eq!(state.workflows.len(), SessionState::MAX_SKILLS_PER_SESSION);
        assert!(!state.workflows.contains_key("overflow_skill"));
        // But active_skill is still set for routing
        assert_eq!(state.active_skill.as_deref(), Some("overflow_skill"));
    }

    // ─── submission failure tracking ──────────────────────────────────────

    #[test]
    fn cooldown_remaining_none_for_unseen_phase_key() {
        let state = SessionState::new("sess-cd-1");
        assert_eq!(
            state.submission_cooldown_remaining("linear:claim", 3, 30),
            None,
            "an unseen phase_key has no failure history → no cooldown"
        );
    }

    #[test]
    fn record_then_cooldown_blocks_briefly() {
        let mut state = SessionState::new("sess-cd-2");
        state.record_submission_failure("linear:claim");
        // base_cooldown=30s and we just stamped now → there must be cooldown
        let remaining = state
            .submission_cooldown_remaining("linear:claim", 3, 30)
            .expect("cooldown active immediately after a failure");
        assert!(remaining > 0 && remaining <= 30);
    }

    #[test]
    fn linear_backoff_kicks_in_at_max_rapid() {
        let mut state = SessionState::new("sess-cd-3");
        // 3 failures = MAX_RAPID — backoff should multiply by count
        for _ in 0..3 {
            state.record_submission_failure("linear:claim");
        }
        let remaining = state
            .submission_cooldown_remaining("linear:claim", 3, 30)
            .expect("3 failures still inside the multiplied window");
        // 3 × 30 = 90s window minus a few ms of test clock drift
        assert!(
            remaining > 80 && remaining <= 90,
            "expected ~90s remaining, got {remaining}"
        );
    }

    #[test]
    fn clear_submission_failure_resets() {
        let mut state = SessionState::new("sess-cd-4");
        state.record_submission_failure("linear:claim");
        assert!(state.submission_attempts("linear:claim").is_some());
        state.clear_submission_failure("linear:claim");
        assert!(state.submission_attempts("linear:claim").is_none());
        assert_eq!(
            state.submission_cooldown_remaining("linear:claim", 3, 30),
            None
        );
    }

    #[test]
    fn submission_attempts_count_increments() {
        let mut state = SessionState::new("sess-cd-5");
        for expected in 1..=4_u32 {
            state.record_submission_failure("linear:claim");
            assert_eq!(
                state.submission_attempts("linear:claim").map(|a| a.count),
                Some(expected),
            );
        }
    }
}
