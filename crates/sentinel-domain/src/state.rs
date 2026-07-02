//! Session State
//!
//! In-memory state shared across hook engine, MCP server, and local API.
//! This is the single source of truth for a running sentinel daemon.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::evidence::EvidenceCollector;
use crate::proof::{PhaseProof, ProofChain, ProofChainError, GENESIS_HASH};
use crate::step_proof::StepProof;
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

    /// Workflow states projected from durable `LangGraph` checkpoints.
    ///
    /// This is intentionally in-memory only. Session JSON never stores workflow
    /// progress; every runtime surface must re-project from the checkpoint
    /// database or mutate through the graph authority.
    #[serde(skip)]
    langgraph_workflows: HashMap<String, WorkflowState>,

    /// Proof chains per skill.
    ///
    /// Kept private so production callers cannot mutate proof-chain state
    /// outside the graph-backed proof engine path. Use the read accessors for
    /// API views, and the explicit append/restore methods for the
    /// few trusted write paths.
    proof_chains: HashMap<String, ProofChain>,

    /// Hook execution counts
    pub hook_stats: HookStats,

    /// Whether the session is still active
    pub active: bool,

    /// Phase files that have been `Read()` by Claude, keyed by skill name.
    /// E.g., `{"linear": ["claim.md", "fetch.md"], "browserbase": ["setup.md"]}`.
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

    /// Session-wide **production override**. When `Some`, the operator has
    /// armed prod work by saying "production override"; prod actions (deploys,
    /// prod Doppler/Auth0, destructive prod, prod DB ops/migrations) are
    /// authorized for the rest of the session without per-action asking, each
    /// surfaced via a dual-display notice. Cleared by "production lock" or
    /// session end. `None` = the default prod-refusal posture.
    #[serde(default)]
    pub production_override: Option<ProductionOverride>,

    /// **Agent revocation kill switch (AEGIS pattern)**: agent IDs that
    /// have been explicitly revoked via `sentinel agent revoke <id>` or
    /// auto-revoked by the violation policy. Tool calls bearing these
    /// `agent_ids` in `HookInput.agent_id` are denied at `PreToolUse` with
    /// a `[Sentinel-Authority]` message.
    ///
    /// Per-session today; revocation does NOT persist across `SessionStart`
    /// because a fresh session is the natural place to give an agent
    /// another chance. Operators who want durable revocations can
    /// re-issue `sentinel agent revoke` at `SessionStart` via a hook.
    #[serde(default)]
    pub revoked_agents: HashSet<String>,

    /// **Cold-start baseline (M1.8 — AEGIS pattern)**: per-step counts of
    /// successful judgements observed *before* enforcement engages. Keyed
    /// by `"<skill>:<phase_id>:<step_id>"`. Lets new skills run their
    /// first N executions in observation mode — judge runs and produces
    /// verdicts, but verdicts don't gate chain progression until the
    /// step has accumulated `baseline_threshold` successful judgements.
    ///
    /// Prevents new skills from being unusable day-one due to over-strict
    /// initial AI judgements (the typical pattern in adversarial judges
    /// is high false-positive on early data, then calibration). Borrowed
    /// directly from AEGIS's 200-trace cold-start safety.
    ///
    /// Today this is per-session — new sessions start fresh. Cross-session
    /// baseline persistence is filed as a follow-up that depends on #78
    /// (proof chain persistence) since both walk the same disk archive.
    #[serde(default)]
    pub step_baselines: HashMap<String, BaselineCounter>,

    /// Independent step verdicts produced by the `step_judge` `PostToolUse`
    /// hook, keyed by `skill:phase_id:step_id` (#12 — close the self-certify
    /// gap). `submit_step_complete` reads these to enforce the judge's OWN
    /// verdict over the caller-supplied one: an agent can pass any `verdict`
    /// arg, but the independently-judged verdict here is what gates the seal
    /// in warn/enforce mode. The hook persists this to disk via `state_store`;
    /// the MCP handler sees it because `with_session_state` loads the same
    /// per-session state before running the tool.
    #[serde(default)]
    pub independent_verdicts: HashMap<String, IndependentVerdict>,
}

/// Counter tracking successful judgements for a single
/// `(skill, phase_id, step_id)` tuple. Used by `step_judge` to decide
/// whether the step has cleared its cold-start window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BaselineCounter {
    /// Number of successful (sufficient = true) judgements observed for
    /// this step in the current baseline window.
    #[serde(default)]
    pub successful_count: u64,

    /// Number of insufficient judgements observed during warmup.
    /// Tracked for telemetry — high counts during warmup signal the
    /// judge prompt or evidence shape may need iteration before
    /// enforcement engages.
    #[serde(default)]
    pub insufficient_count: u64,

    /// Wall-clock timestamp of the most recent judgement. Helps
    /// diagnose stalled baselines (a step that hasn't run in weeks
    /// vs one in active warmup).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed_at: Option<DateTime<Utc>>,
}

/// The independent verdict the `step_judge` hook produced for a step,
/// persisted so `submit_step_complete` can enforce it over the
/// caller-supplied verdict (#12). Minimal by design — only what the seal
/// gate needs to decide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndependentVerdict {
    /// Whether the independent judge found the evidence sufficient.
    pub sufficient: bool,
    /// The independent judge's confidence in `sufficient`.
    pub confidence: f64,
    /// When the hook recorded this verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged_at: Option<DateTime<Utc>>,
}

impl BaselineCounter {
    /// Returns true if this counter has cleared the threshold for
    /// enforcement to engage. `threshold == 0` means "enforce
    /// immediately" — return true even on a fresh counter.
    #[must_use]
    pub const fn cleared(&self, threshold: u64) -> bool {
        self.successful_count >= threshold
    }
}

/// Session-wide production-override grant. Armed by the operator saying
/// "production override"; cleared by "production lock" or session end.
/// Deliberately simple (no challenge code / expiry) — it's an operator-
/// driven, session-scoped grant surfaced via a dual-display notice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionOverride {
    /// When the operator armed prod work (RFC3339 UTC).
    pub armed_at: DateTime<Utc>,
    /// Optional operator-supplied note captured alongside the phrase
    /// (e.g. "production override — hotfix the auth migration").
    pub note: Option<String>,
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
            langgraph_workflows: HashMap::new(),
            proof_chains: HashMap::new(),
            hook_stats: HookStats::default(),
            active: true,
            phases_read: HashMap::new(),
            tool_calls: 0,
            failed_submissions: HashMap::new(),
            phase_file_hashes: HashMap::new(),
            state_generation: 0,
            production_override: None,
            step_baselines: HashMap::new(),
            independent_verdicts: HashMap::new(),
            revoked_agents: HashSet::new(),
        }
    }

    /// Arm the session-wide production override (operator requested "production
    /// override"). Idempotent — re-arming refreshes `armed_at`/`note`.
    pub fn arm_production_override(&mut self, note: Option<String>) {
        self.production_override = Some(ProductionOverride {
            armed_at: Utc::now(),
            note,
        });
    }

    /// Revoke the production override (operator requested "production lock", or a
    /// fresh lock is desired). No-op if not armed.
    pub fn revoke_production_override(&mut self) {
        self.production_override = None;
    }

    /// Whether prod actions are currently authorized for this session.
    #[must_use]
    pub const fn production_override_armed(&self) -> bool {
        self.production_override.is_some()
    }

    /// Revoke an agent — every subsequent tool call carrying this
    /// `agent_id` will be denied at `PreToolUse`. Idempotent (revoking
    /// an already-revoked agent is a no-op).
    pub fn revoke_agent(&mut self, agent_id: impl Into<String>) {
        self.revoked_agents.insert(agent_id.into());
    }

    /// Lift a revocation. Mostly useful in tests / interactive recovery
    /// from operator error; production paths should rarely call this.
    pub fn unrevoke_agent(&mut self, agent_id: &str) -> bool {
        self.revoked_agents.remove(agent_id)
    }

    /// Check whether an `agent_id` has been revoked.
    #[must_use]
    pub fn is_agent_revoked(&self, agent_id: &str) -> bool {
        self.revoked_agents.contains(agent_id)
    }

    /// Build the `(skill, phase_id, step_id)` baseline key. Static helper
    /// so both `step_judge` and the persistence layer compute keys
    /// identically.
    #[must_use]
    pub fn baseline_key(skill: &str, phase_id: &str, step_id: &str) -> String {
        format!("{skill}:{phase_id}:{step_id}")
    }

    /// Record a step judgement against the cold-start baseline.
    /// Called by `step_judge` after every verdict — both passing and failing
    /// judgements bump their respective counters so telemetry shows
    /// warmup-time false-positive rates.
    ///
    /// Returns the post-update counter so callers can decide whether
    /// the step has cleared its threshold.
    pub fn record_step_judgement(
        &mut self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        sufficient: bool,
    ) -> BaselineCounter {
        let key = Self::baseline_key(skill, phase_id, step_id);
        let counter = self.step_baselines.entry(key).or_default();
        if sufficient {
            counter.successful_count = counter.successful_count.saturating_add(1);
        } else {
            counter.insufficient_count = counter.insufficient_count.saturating_add(1);
        }
        counter.last_observed_at = Some(Utc::now());
        counter.clone()
    }

    /// Read the baseline counter for a step without mutating it.
    /// Returns the default (zero) counter when no judgements have been
    /// recorded yet — callers see "no observations yet" as the natural
    /// initial state, not None.
    #[must_use]
    pub fn step_baseline(&self, skill: &str, phase_id: &str, step_id: &str) -> BaselineCounter {
        self.step_baselines
            .get(&Self::baseline_key(skill, phase_id, step_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Record the independent `step_judge` verdict for a step (#12). Keyed
    /// the same way as the baseline counter. Overwrites any prior verdict
    /// for the step — the most recent independent judgement is what gates
    /// the seal. Called by the `step_judge` hook after every judgement.
    pub fn record_independent_verdict(
        &mut self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        sufficient: bool,
        confidence: f64,
    ) {
        let key = Self::baseline_key(skill, phase_id, step_id);
        self.independent_verdicts.insert(
            key,
            IndependentVerdict {
                sufficient,
                confidence,
                judged_at: Some(Utc::now()),
            },
        );
    }

    /// Read the independent verdict for a step, if the `step_judge` hook
    /// recorded one. `None` means no independent judgement exists for this
    /// step; production seal gates must reject that instead of falling back to
    /// caller-supplied self-certification.
    #[must_use]
    pub fn independent_verdict(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
    ) -> Option<&IndependentVerdict> {
        self.independent_verdicts
            .get(&Self::baseline_key(skill, phase_id, step_id))
    }

    /// Set the active skill marker (from skill router).
    ///
    /// This intentionally does not allocate [`WorkflowState`] or [`ProofChain`]
    /// entries. Configured workflow state is owned by the durable `LangGraph`
    /// authority and must be inserted only from a graph checkpoint projection.
    pub fn set_active_skill(&mut self, skill: impl Into<String>) {
        self.set_active_skill_marker(skill);
    }

    /// Set only the active skill marker without allocating workflow/proof state.
    ///
    /// Graph-owned callers use this when `LangGraph` is the workflow authority:
    /// the projected workflow/proof entries are written only after the graph
    /// accepts the transition.
    pub fn set_active_skill_marker(&mut self, skill: impl Into<String>) {
        self.active_skill = Some(skill.into());
    }

    /// Store a workflow state that came from the durable `LangGraph` authority.
    pub fn set_graph_projected_workflow(
        &mut self,
        skill: impl Into<String>,
        workflow_state: WorkflowState,
    ) {
        let skill = skill.into();
        assert_eq!(
            workflow_state.skill, skill,
            "graph-projected workflow skill must match the session-state key"
        );
        self.langgraph_workflows.insert(skill, workflow_state);
    }

    /// Remove graph-authoritative workflow projection for a skill.
    pub fn remove_graph_projected_workflow(&mut self, skill: &str) -> Option<WorkflowState> {
        self.langgraph_workflows.remove(skill)
    }

    /// Remove graph workflow state that is no longer configured.
    pub fn retain_configured_graph_workflows(&mut self, is_configured: impl Fn(&str) -> bool) {
        self.langgraph_workflows
            .retain(|skill, _| is_configured(skill));
    }

    /// Number of graph workflow entries.
    #[must_use]
    pub fn graph_workflow_count(&self) -> usize {
        self.langgraph_workflows.len()
    }

    /// Get a workflow state projected from `LangGraph`.
    #[must_use]
    pub fn graph_workflow(&self, skill: &str) -> Option<&WorkflowState> {
        self.langgraph_workflows.get(skill)
    }

    /// Get mutable workflow state projected from `LangGraph`.
    #[cfg(test)]
    pub fn graph_workflow_mut(&mut self, skill: &str) -> Option<&mut WorkflowState> {
        self.langgraph_workflows.get_mut(skill)
    }

    /// Whether a skill has graph-authoritative workflow state.
    #[must_use]
    pub fn has_graph_workflow(&self, skill: &str) -> bool {
        self.graph_workflow(skill).is_some()
    }

    /// Whether any workflow state in this session is graph-authoritative.
    #[must_use]
    pub fn has_any_graph_workflow(&self) -> bool {
        self.graph_workflows().next().is_some()
    }

    /// Iterate only graph-authoritative workflow states.
    pub fn graph_workflows(&self) -> impl Iterator<Item = (&String, &WorkflowState)> {
        self.langgraph_workflows.iter()
    }

    /// Get the graph-authoritative workflow state for the active skill.
    #[must_use]
    pub fn active_workflow(&self) -> Option<&WorkflowState> {
        self.active_skill
            .as_deref()
            .and_then(|s| self.graph_workflow(s))
    }

    /// Get the proof chain for the active skill
    #[must_use]
    pub fn active_proof_chain(&self) -> Option<&ProofChain> {
        self.active_skill
            .as_ref()
            .and_then(|s| self.proof_chains.get(s))
    }

    /// Get a proof chain by skill.
    #[must_use]
    pub fn proof_chain(&self, skill: &str) -> Option<&ProofChain> {
        self.proof_chains.get(skill)
    }

    /// Whether a proof chain exists for a skill.
    #[must_use]
    pub fn has_proof_chain(&self, skill: &str) -> bool {
        self.proof_chains.contains_key(skill)
    }

    /// Whether there are no proof chains in this session.
    #[must_use]
    pub fn proof_chains_is_empty(&self) -> bool {
        self.proof_chains.is_empty()
    }

    /// Number of proof chains in this session.
    #[must_use]
    pub fn proof_chain_count(&self) -> usize {
        self.proof_chains.len()
    }

    /// Iterate proof-chain skills.
    pub fn proof_chain_skills(&self) -> impl Iterator<Item = &String> {
        self.proof_chains.keys()
    }

    /// Iterate proof chains.
    pub fn proof_chains(&self) -> impl Iterator<Item = (&String, &ProofChain)> {
        self.proof_chains.iter()
    }

    /// Return a skill's proof-chain head hash, or genesis for an empty chain.
    #[must_use]
    pub fn proof_chain_head_hash(&self, skill: &str) -> &str {
        self.proof_chains
            .get(skill)
            .map_or(GENESIS_HASH, ProofChain::head_hash)
    }

    /// Append a phase proof through the trusted proof-engine path.
    ///
    /// This is intentionally narrower than exposing the underlying map or a
    /// mutable chain reference: callers still get `ProofChain`'s link and
    /// self-verification checks, but cannot arbitrarily rewrite the chain.
    pub fn append_phase_proof(
        &mut self,
        skill: impl Into<String>,
        proof: PhaseProof,
    ) -> Result<(), ProofChainError> {
        let skill = skill.into();
        let session_id = self.session_id.clone();
        let chain = self
            .proof_chains
            .entry(skill.clone())
            .or_insert_with(|| ProofChain::new(skill, session_id));
        chain.add_phase_entry(proof)
    }

    /// Append a step proof through the trusted proof-engine path.
    pub fn append_step_proof(
        &mut self,
        skill: impl Into<String>,
        proof: StepProof,
    ) -> Result<(), ProofChainError> {
        let skill = skill.into();
        let session_id = self.session_id.clone();
        let chain = self
            .proof_chains
            .entry(skill.clone())
            .or_insert_with(|| ProofChain::new(skill, session_id));
        chain.add_step_proof(proof)
    }

    /// Restore a complete proof chain from trusted persistence or test setup.
    ///
    /// Runtime sealing should use `append_phase_proof`/`append_step_proof`
    /// instead so the chain link is validated at the mutation boundary.
    pub fn restore_proof_chain(&mut self, skill: impl Into<String>, chain: ProofChain) {
        let skill = skill.into();
        assert_eq!(
            chain.skill, skill,
            "restored proof-chain skill must match the session-state key"
        );
        self.proof_chains.insert(skill, chain);
    }

    /// Get mutable graph-authoritative workflow state for the active skill.
    #[cfg(test)]
    pub fn active_workflow_mut(&mut self) -> Option<&mut WorkflowState> {
        self.active_skill
            .clone()
            .and_then(move |s| self.graph_workflow_mut(&s))
    }

    /// Get mutable proof chain for the active skill.
    #[cfg(test)]
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
        assert_eq!(state.graph_workflow_count(), 0);
    }

    #[test]
    fn test_set_active_skill() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");

        assert_eq!(state.active_skill.as_deref(), Some("linear"));
        assert!(
            !state.has_any_graph_workflow(),
            "set_active_skill must not synthesize workflow state"
        );
        assert!(
            state.proof_chains.is_empty(),
            "set_active_skill must not synthesize proof chains"
        );
    }

    #[test]
    fn test_active_workflow() {
        let mut state = SessionState::new("sess-1");
        assert!(state.active_workflow().is_none());

        state.set_active_skill("linear");
        assert!(
            state.active_workflow().is_none(),
            "marker-only active skill must not create workflow state"
        );

        let mut value = serde_json::to_value(&state).expect("session state serializes");
        value
            .as_object_mut()
            .expect("session state is an object")
            .insert(
                "workflows".to_string(),
                serde_json::json!({
                    "linear": WorkflowState::new("linear", "sess-1")
                }),
            );
        state = serde_json::from_value(value).expect("session state deserializes");
        state.set_active_skill("linear");
        assert!(
            state.active_workflow().is_none(),
            "old workflows state is not LangGraph authority"
        );
        assert!(
            state.graph_workflow("linear").is_none(),
            "graph workflow accessor must ignore old workflows state"
        );

        state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess-1"));
        assert!(state.active_workflow().is_some());
        assert_eq!(state.active_workflow().unwrap().skill, "linear");
    }

    #[test]
    fn mutable_workflow_accessor_requires_graph_projection() {
        let mut state = SessionState::new("sess-1");

        assert!(
            state.graph_workflow_mut("linear").is_none(),
            "missing graph workflow state must not be mutable through graph accessors"
        );

        state.remove_graph_projected_workflow("linear");
        state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess-1"));
        state
            .graph_workflow_mut("linear")
            .expect("graph-projected workflow")
            .current_phase = Some(0);

        assert_eq!(
            state.graph_workflow("linear").unwrap().current_phase,
            Some(0)
        );
    }

    #[test]
    fn mutable_proof_chain_accessor_requires_existing_active_chain() {
        let mut state = SessionState::new("sess-1");
        state.set_active_skill("linear");

        assert!(
            state.active_proof_chain_mut().is_none(),
            "active skill marker alone must not synthesize mutable proof-chain state"
        );

        state
            .proof_chains
            .insert("linear".to_string(), ProofChain::new("linear", "sess-1"));

        assert!(
            state.active_proof_chain_mut().is_some(),
            "test-only mutable proof-chain accessor should expose existing active chain"
        );
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
        state.set_graph_projected_workflow("linear", WorkflowState::new("linear", "sess-1"));

        // Advance workflow — need a workflow definition for advance_sequential
        let wf = crate::workflow::SkillWorkflow {
            skill: "linear".to_string(),
            phases: vec![crate::workflow::WorkflowPhase {
                id: "claim".to_string(),
                file: "claim.md".to_string(),
                required: true,
                judge: crate::judge::JudgeModel::Sonnet,
                description: "Claim".to_string(),
                required_dyad: None,
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
        assert!(!state.has_phase_been_read("browserbase", "claim.md")); // per-skill isolation

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
                    required_dyad: None,
                },
                WorkflowPhase {
                    id: "fetch".to_string(),
                    file: "fetch.md".to_string(),
                    required: true,
                    judge: JudgeModel::Sonnet,
                    description: String::new(),
                    required_dyad: None,
                },
                WorkflowPhase {
                    id: "cleanup".to_string(),
                    file: "cleanup.md".to_string(),
                    required: false,
                    judge: JudgeModel::Sonnet,
                    description: String::new(),
                    required_dyad: None,
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
    fn test_active_skill_markers_do_not_allocate_workflows() {
        let mut state = SessionState::new("sess-1");
        for i in 0..150 {
            state.set_active_skill(format!("skill_{i}"));
        }
        assert!(
            !state.has_any_graph_workflow(),
            "active skill markers must not allocate unbounded workflow state"
        );
        assert_eq!(state.active_skill.as_deref(), Some("skill_149"));
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
