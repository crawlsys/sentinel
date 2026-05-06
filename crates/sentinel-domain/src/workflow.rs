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

/// **Retry policy (M4.4 — Apollo runtime resilience)**.
/// Configures automatic retry behavior for a single step. Consumed by
/// skills-mcp (M2) at execution time — sentinel-domain just carries the
/// declared policy.
///
/// `Default::default()` returns the no-retry policy (max_attempts=1,
/// backoff_ms=100, no retry_on filter). The `#[serde(default = ...)]`
/// attributes on individual fields apply to deserialization only —
/// the manual `Default` impl below ensures `Default::default()` matches
/// the deserialization behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of attempts (1 = no retries, 3 = original + 2 retries).
    /// Default 1 (no retries) — opt-in for steps where transient failures
    /// are expected (network calls, external API hits).
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,

    /// Initial backoff in milliseconds. Exponential growth between
    /// attempts: backoff_ms, backoff_ms*2, backoff_ms*4, ...
    /// Default 100ms.
    #[serde(default = "default_backoff_ms")]
    pub backoff_ms: u64,

    /// Optional list of error categories to retry on. Empty list means
    /// "retry on any error." When non-empty, the step must classify its
    /// failure into one of these categories for the retry to fire.
    /// Categories are skill-defined strings (e.g. "transient",
    /// "rate-limit", "timeout").
    #[serde(default)]
    pub retry_on: Vec<String>,
}

fn default_max_attempts() -> u32 {
    1
}
fn default_backoff_ms() -> u64 {
    100
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            backoff_ms: default_backoff_ms(),
            retry_on: Vec::new(),
        }
    }
}

impl RetryPolicy {
    /// Returns true if this policy actually retries (max_attempts > 1).
    /// Steps without a retry_policy in TOML get the default which is
    /// "no retry" — `should_retry()` short-circuits cleanly for them.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.max_attempts > 1
    }

    /// Compute the backoff (ms) for attempt N (1-indexed). Returns 0
    /// for the first attempt (no wait); exponential thereafter capped
    /// at 60 seconds to prevent pathological waits.
    #[must_use]
    pub fn backoff_for_attempt(&self, attempt: u32) -> u64 {
        if attempt <= 1 {
            return 0;
        }
        let raw = self.backoff_ms.saturating_mul(1u64 << (attempt - 2).min(20));
        raw.min(60_000)
    }
}

/// **Circuit breaker (M4.4 — Apollo runtime resilience)**.
/// After N consecutive failures of this step, the skill MCP is
/// circuit-broken: subsequent invocations short-circuit to a "circuit
/// open" error without running the step body. The router can then fall
/// back to alternative steps. After `cooldown_ms`, the breaker
/// half-opens — the next invocation runs and either resets the breaker
/// (success) or re-trips it (failure).
///
/// `Default::default()` returns the disabled breaker
/// (failure_threshold=0, cooldown_ms=30000). Manual impl ensures the
/// cooldown matches the serde-default value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CircuitBreaker {
    /// Consecutive failures before the breaker trips. Default 0 means
    /// "no circuit breaker" — opt-in only.
    #[serde(default)]
    pub failure_threshold: u32,

    /// Wall-clock cooldown in milliseconds before the breaker
    /// half-opens. Default 30 seconds.
    #[serde(default = "default_cooldown_ms")]
    pub cooldown_ms: u64,
}

fn default_cooldown_ms() -> u64 {
    30_000
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self {
            failure_threshold: 0,
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

impl CircuitBreaker {
    /// Returns true if this breaker is configured (failure_threshold > 0).
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.failure_threshold > 0
    }
}

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

    /// **Cold-start baseline threshold (M1.8 — AEGIS pattern)**: number
    /// of successful judgements this step must accumulate before the
    /// AI judge's verdict starts gating chain progression. Below the
    /// threshold, the judge runs and produces verdicts but they are
    /// observational — the chain doesn't refuse to seal on early
    /// over-strict negatives.
    ///
    /// Default `0` means "enforce immediately" — right for high-stakes
    /// steps (deploy_prod, prod_migration) where we'd rather refuse a
    /// real action than let unproven evidence through. Routine steps
    /// can set this to `5` or `10` so first-runs in fresh skills don't
    /// hit cold-start false-positives.
    #[serde(default)]
    pub baseline_threshold: u64,

    /// **Per-step timeout (M4.4 — Apollo resilience)**. Wall-clock cap
    /// in milliseconds. Skills-mcp aborts the step if exceeded. None =
    /// no timeout (the default) — appropriate for steps with their own
    /// internal timeout management or steps where wall-clock isn't the
    /// right axis to bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    /// **Retry policy (M4.4)**. See [`RetryPolicy`]. Default is the
    /// no-retry policy (max_attempts=1) — opt-in for steps where
    /// transient failures are expected.
    #[serde(default)]
    pub retry_policy: RetryPolicy,

    /// **Circuit breaker (M4.4)**. See [`CircuitBreaker`]. Default
    /// (failure_threshold=0) means no breaker — opt-in for skills
    /// that may degrade and need fallback routing.
    #[serde(default)]
    pub circuit_breaker: CircuitBreaker,

    // ─── Apollo Federation directives (M2.5) ────────────────────────
    //
    // Cross-skill handoff contracts for the federated step namespace.
    // All four are optional; existing TOML configs without them load
    // unchanged. Future federation compose validation passes consult
    // these to verify cross-skill contracts hold.

    /// **`@provides`** — artifact types this step's StepProof exposes
    /// for downstream consumers. Strings are skill-namespaced (e.g.
    /// `"linear.ticket_id"`, `"git.pr_url"`). When other skills declare
    /// `requires` of the same string, federation compose can verify
    /// the producer-consumer chain at composition time, before any
    /// chain runs.
    #[serde(default)]
    pub provides: Vec<String>,

    /// **`@requires`** — artifact types from other skills' steps that
    /// this step consumes. Inverse of `provides`. Federation compose
    /// errors when a step requires an artifact no other step provides.
    #[serde(default)]
    pub requires: Vec<String>,

    /// **`@external`** — references to other skills' step IDs that
    /// this step's logic depends on (e.g. `"git.create_pr.4"`).
    /// Different from `requires`: requires is the data shape contract,
    /// external is the execution-order contract — "this step assumes
    /// step X has run." Federation compose can verify the referenced
    /// step exists.
    #[serde(default)]
    pub external: Vec<String>,

    /// **`@inaccessible`** — internal-only step, not callable by the
    /// router. Useful for skill-internal helpers that other steps in
    /// the same skill chain into but the federated supergraph
    /// shouldn't expose to virtual skill packs (M7). Default false.
    #[serde(default)]
    pub inaccessible: bool,

    // ─── Apollo Federation deprecation/migration (M2.6) ─────────────
    //
    // Apollo's `@deprecated` directive carries a reason string; we
    // mirror that and add `@override` for explicit replacement
    // declarations. Both default to None so existing TOML loads
    // unchanged.

    /// **`@deprecated`** — when `Some(reason)`, this step is on its
    /// way out and operators should migrate. The reason is shown by
    /// federation compose as a warning (not an error — deprecated
    /// steps still function). To mark a step as un-deprecated, omit
    /// the field or set it to `None`.
    #[serde(default)]
    pub deprecated: Option<String>,

    /// **`@override`** — names a step this one replaces. Form:
    /// `"phase.step_id"` (same skill, the common case) or
    /// `"skill.phase.step_id"` (cross-skill, when one skill takes
    /// over capability previously owned by another).
    ///
    /// Federation compose errors on dangling override targets and
    /// warns when the target isn't itself marked `deprecated` —
    /// disciplined migration paths declare the deprecation up-front
    /// so consumers know the contract is changing.
    #[serde(default)]
    pub r#override: Option<String>,
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

/// Default federation version for SkillSteps configs that pre-date M2.7.
/// Pre-M2.7 configs have no `federation_version` field; serde fills in
/// `"1"` so they keep loading and the rest of the system can assume the
/// field is always present.
fn default_federation_version() -> String {
    "1".to_string()
}

/// All step definitions for a skill
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSteps {
    /// Skill name — skipped during serde because it is set by
    /// `config::load_skill_steps()` from the filename, not from TOML content.
    /// This struct is never round-tripped through JSON.
    #[serde(skip)]
    pub skill: String,

    /// **Federation version (M2.7 — Apollo schema versioning lesson)**.
    /// Bumped on breaking changes to the skill's federated step namespace
    /// (handoff signature change, step removal, phase removal, anything
    /// that would invalidate a chain composed against the prior version).
    /// Non-breaking changes (added optional fields, new steps, new phases,
    /// description edits, tag edits) keep the same version.
    ///
    /// Defaults to `"1"` for configs written before M2.7 — they keep
    /// loading without modification. New configs should set this
    /// explicitly so authors think about the contract from the start.
    ///
    /// `sentinel federation check` (M2.8) compares versions across
    /// branches and refuses to allow breaking changes without a bump.
    #[serde(default = "default_federation_version")]
    pub federation_version: String,

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

const fn default_true() -> bool {
    true
}

const fn default_judge() -> JudgeModel {
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
        workflow
            .phases
            .iter()
            .find(|&phase| phase.required && !self.is_phase_complete(&phase.id))
            .map(|v| v as _)
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
        //   EnterPlanMode — transitions the session into plan mode (a permission-mode
        //     change, not code execution). This tool is omitted from the public
        //     `package/sdk-tools.d.ts` type declaration but IS a real callable tool
        //     in the compiled binary (confirmed in claude-code-2.1.114 decompile:
        //     handler `r7H` at line 1666; rejects inside agent contexts). Keeping
        //     it exempt so the model can always opt into plan mode without being
        //     gated by phase progress.
        //   ExitPlanMode — writes a plan file + requests approval, no side-effectful work
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
            "EnterPlanMode",
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

    // ── M4.4 RetryPolicy / CircuitBreaker tests ─────────────────────

    #[test]
    fn retry_policy_default_has_no_retries() {
        // Default policy = max_attempts: 1 = no retries. Steps without
        // a retry_policy in TOML get this; behavior must be "execute
        // once, never retry."
        let p = RetryPolicy::default();
        assert_eq!(p.max_attempts, 1);
        assert!(!p.enabled(), "default policy must report not-enabled");
    }

    #[test]
    fn retry_policy_enabled_when_max_attempts_above_one() {
        let p = RetryPolicy {
            max_attempts: 3,
            backoff_ms: 100,
            retry_on: vec![],
        };
        assert!(p.enabled());
    }

    #[test]
    fn retry_backoff_first_attempt_is_zero() {
        // Attempt 1 = the original call, so no wait before it.
        let p = RetryPolicy {
            max_attempts: 5,
            backoff_ms: 100,
            retry_on: vec![],
        };
        assert_eq!(p.backoff_for_attempt(1), 0);
        assert_eq!(p.backoff_for_attempt(0), 0, "attempt 0 also no-wait");
    }

    #[test]
    fn retry_backoff_grows_exponentially() {
        let p = RetryPolicy {
            max_attempts: 10,
            backoff_ms: 100,
            retry_on: vec![],
        };
        // Attempt 2 = first retry: backoff_ms * 2^0 = 100
        // Attempt 3 = second retry: backoff_ms * 2^1 = 200
        // Attempt 4: 400, etc.
        assert_eq!(p.backoff_for_attempt(2), 100);
        assert_eq!(p.backoff_for_attempt(3), 200);
        assert_eq!(p.backoff_for_attempt(4), 400);
        assert_eq!(p.backoff_for_attempt(5), 800);
    }

    #[test]
    fn retry_backoff_caps_at_60_seconds() {
        // Pathological case: backoff_ms=1000, attempt 30 would be
        // 1000 * 2^28 ms ≈ 2.7 trillion ms (~85 years). Cap at 60s
        // so a misconfigured retry doesn't hang forever.
        let p = RetryPolicy {
            max_attempts: 100,
            backoff_ms: 1000,
            retry_on: vec![],
        };
        let huge = p.backoff_for_attempt(30);
        assert_eq!(huge, 60_000, "backoff must cap at 60s, got {huge}");
    }

    #[test]
    fn retry_backoff_handles_overflow_safely() {
        // saturating_mul + the 1<<20 cap on the shift amount must
        // prevent integer overflow even at hostile input sizes.
        let p = RetryPolicy {
            max_attempts: u32::MAX,
            backoff_ms: u64::MAX,
            retry_on: vec![],
        };
        // Doesn't panic.
        let _ = p.backoff_for_attempt(u32::MAX);
    }

    #[test]
    fn circuit_breaker_default_disabled() {
        let cb = CircuitBreaker::default();
        assert_eq!(cb.failure_threshold, 0);
        assert!(!cb.enabled(), "default breaker must report not-enabled");
    }

    #[test]
    fn circuit_breaker_enabled_when_threshold_above_zero() {
        let cb = CircuitBreaker {
            failure_threshold: 5,
            cooldown_ms: 30_000,
        };
        assert!(cb.enabled());
    }

    #[test]
    fn workflow_step_loads_with_no_resilience_fields() {
        // Backwards compat: step config without timeout/retry/breaker
        // fields must serde-default to no-timeout / no-retry /
        // no-breaker. Existing skill configs (from before M4.4) get
        // the safe-default "execute once with no resilience" behavior.
        // Using JSON round-trip here (rather than TOML) because
        // sentinel-domain doesn't depend on the toml crate; the
        // serde-default property is format-agnostic.
        let json = serde_json::json!({
            "id": "1",
            "description": "fetch ticket",
        });
        let step: WorkflowStep = serde_json::from_value(json).expect("loads");
        assert_eq!(step.id, "1");
        assert!(step.timeout_ms.is_none());
        assert!(!step.retry_policy.enabled());
        assert!(!step.circuit_breaker.enabled());
        assert_eq!(step.baseline_threshold, 0);
    }

    #[test]
    fn workflow_step_loads_with_resilience_fields() {
        // New skill configs that opt into M4.4 see their declared
        // values preserved through the loader.
        let json = serde_json::json!({
            "id": "deploy_prod",
            "description": "Deploy to production",
            "blocker": true,
            "timeout_ms": 60000_u64,
            "retry_policy": {
                "max_attempts": 3,
                "backoff_ms": 500,
                "retry_on": ["transient", "rate-limit"],
            },
            "circuit_breaker": {
                "failure_threshold": 3,
                "cooldown_ms": 60000_u64,
            },
        });
        let step: WorkflowStep = serde_json::from_value(json).expect("loads");
        assert_eq!(step.timeout_ms, Some(60_000));
        assert_eq!(step.retry_policy.max_attempts, 3);
        assert_eq!(step.retry_policy.backoff_ms, 500);
        assert_eq!(step.retry_policy.retry_on, vec!["transient", "rate-limit"]);
        assert_eq!(step.circuit_breaker.failure_threshold, 3);
        assert_eq!(step.circuit_breaker.cooldown_ms, 60_000);
    }

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

        // `EnterPlanMode` IS a real callable tool in the compiled Claude Code
        // binary (2.1.114 decompile: handler `r7H` at line 1666), despite being
        // omitted from the public `sdk-tools.d.ts` type union. Must be exempt
        // so plan-mode entry isn't phase-gated.
        assert!(state.should_block(&wf, "EnterPlanMode").is_none(),
            "EnterPlanMode must be exempt — real tool in compiled binary, just hidden from sdk-tools.d.ts");
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
