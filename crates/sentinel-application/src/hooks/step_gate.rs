//! Step Gate Hook
//!
//! `PreToolUse` hook that blocks fine-grained step tool calls when their
//! prerequisite step has not been completed in the active LangGraph checkpoint
//! and attested by a [`StepProof`] in the active [`ProofChain`].
//!
//! # Relationship to `phase_gate`
//!
//! `phase_gate` enforces *coarse* phase boundaries: did you read the phase
//! file before invoking write/exec tools? `step_gate` layers underneath at
//! finer granularity: within a phase, did the prior step in the configured
//! sequence produce a proof before the next step's tool is called?
//!
//! Both hooks fire on `PreToolUse`. They are **complementary**, not redundant:
//! - Step tools require a step config; otherwise `step_gate` denies the call.
//! - `phase_gate` prevents whole phases from being skipped; `step_gate`
//!   prevents steps within a phase from being skipped.
//!
//! # Tool naming convention
//!
//! Step-gated tools follow the federated namespace produced by `skills-mcp`
//! (M2): `mcp__skills__<skill>__step_<step_id>`. The hook only inspects tool
//! names matching this pattern; everything else is allowed through (other
//! gates handle non-step tool calls).
//!
//! # Enforcement
//!
//! When a step tool is about to fire, the hook:
//! 1. Resolves the configured prerequisite step from the skill's step config
//!    using the previous declared step in the same phase.
//! 2. Requires an active LangGraph workflow projection for the skill.
//! 3. Verifies the prerequisite is completed or skipped in that graph state.
//! 4. Verifies a `StepProof` exists for the prerequisite step.
//! 5. Blocks with a `[Sentinel-Authority]` deny message naming the missing
//!    graph/proof prerequisite if not.
//!
//! # Sentinel authority
//!
//! Block messages carry the `[Sentinel-Authority]` provenance prefix
//! consumed by Claude Code.

use std::collections::HashMap;

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::proof::{ProofChain, ProofEntry};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, StepStatus, WorkflowStep};

/// Parsed components of a `mcp__skills__<skill>__step_<step_id>` tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepToolRef {
    skill: String,
    step_id: String,
}

impl StepToolRef {
    /// Parse `mcp__skills__linear__step_3.L2.3` → `{skill: "linear",
    /// step_id: "3.L2.3"}`.
    ///
    /// Returns `None` for any tool name that doesn't follow the
    /// `mcp__skills__<skill>__step_<step_id>` pattern. Non-step tools fall
    /// through this hook entirely.
    fn parse(tool_name: &str) -> Option<Self> {
        // Required prefix.
        let rest = tool_name.strip_prefix("mcp__skills__")?;
        // After "mcp__skills__" we expect "<skill>__step_<step_id>".
        // Use rsplit_once on "__step_" so step IDs containing "_" survive.
        let (skill, step_id) = rest.rsplit_once("__step_")?;
        if skill.is_empty() || step_id.is_empty() {
            return None;
        }
        Some(Self {
            skill: skill.to_string(),
            step_id: step_id.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepGateDecision {
    Allow,
    AllowFirstStep,
    AllowPrerequisiteProof,
    DenyMissingStepConfig,
    DenyStepNotDeclared,
    DenyMissingGraphWorkflow,
    DenyPrerequisiteNotCompleted,
    DenyMissingProofChain,
    DenyMissingStepProof,
}

#[derive(Debug, Clone)]
pub struct StepGateEvaluation {
    pub tool: Option<String>,
    pub tool_present: bool,
    pub step_tool: bool,
    pub skill: Option<String>,
    pub step_id: Option<String>,
    pub step_config_loaded: bool,
    pub step_declared: bool,
    pub phase_id: Option<String>,
    pub target_step_id: Option<String>,
    pub target_step_description: Option<String>,
    pub prerequisite_present: bool,
    pub prerequisite_step_id: Option<String>,
    pub prerequisite_description: Option<String>,
    pub graph_workflow_present: bool,
    pub prerequisite_graph_completed: bool,
    pub proof_chain_present: bool,
    pub step_proof_present: bool,
    pub should_deny: bool,
    pub decision: StepGateDecision,
}

impl StepGateEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.step_tool
    }
}

struct StepPlanRef<'a> {
    phase_id: &'a str,
    target: &'a WorkflowStep,
    prerequisite: Option<&'a WorkflowStep>,
}

/// Find a target step and its prerequisite within a skill's step config.
///
/// Strategy: the prereq is the immediately-preceding step in declared order
/// within the same phase. First step in a phase has no prereq.
///
/// The lookup returns `None` only when the target step is not configured; that
/// must deny instead of being confused with a configured first step.
fn step_plan_ref<'a>(skill_steps: &'a SkillSteps, target_step_id: &str) -> Option<StepPlanRef<'a>> {
    for phase in &skill_steps.phases {
        let mut prev: Option<&WorkflowStep> = None;
        for step in &phase.steps {
            if step.id == target_step_id {
                return Some(StepPlanRef {
                    phase_id: &phase.phase_id,
                    target: step,
                    prerequisite: prev,
                });
            }
            prev = Some(step);
        }
    }
    None
}

fn graph_step_completed(
    workflow: &sentinel_domain::workflow::WorkflowState,
    phase_id: &str,
    step_id: &str,
) -> bool {
    workflow.step_states.iter().any(|state| {
        state.phase_id == phase_id
            && state.step_id == step_id
            && matches!(state.status, StepStatus::Completed | StepStatus::Skipped)
    })
}

/// Check whether a `StepProof` exists in `chain` for the given
/// `(skill, step_id)` pair.
///
/// Walks the mixed-entry chain (`entries`) — phase-only chains never
/// contain step proofs, so the phase-proof Vec is intentionally ignored here.
fn has_step_proof(chain: &ProofChain, skill: &str, step_id: &str) -> bool {
    chain.entries.iter().any(|e| match e {
        ProofEntry::Step(s) => s.skill == skill && s.step_id == step_id,
        // Phase entries are coarser-grained checkpoints, not step proofs.
        // Disagreement markers (#82 Stage B) are metadata about a prior
        // StepProof — they don't satisfy the step_gate prerequisite check
        // by themselves; the underlying StepProof is what counts.
        ProofEntry::Phase(_) | ProofEntry::Disagreement(_) => false,
    })
}

fn deny_with_context(input: &HookInput, reason: impl Into<String>) -> HookOutput {
    HookOutput::deny(super::block_context::append_block_context(reason, input))
}

/// Process a step-gate hook event (`PreToolUse`).
///
/// Returns:
/// - [`HookOutput::allow`] for tool calls that aren't step tools, for the first
///   step in a phase (no prereq), and for steps whose prereq proof exists.
/// - [`HookOutput::deny`] with a `[Sentinel-Authority]` message when the
///   step config or prereq `StepProof` is missing.
pub fn process(
    input: &HookInput,
    state: &SessionState,
    step_configs: &HashMap<String, SkillSteps>,
) -> HookOutput {
    let evaluation = evaluate(input, state, step_configs);
    output_from_evaluation(input, &evaluation)
}

#[must_use]
pub fn evaluate(
    input: &HookInput,
    state: &SessionState,
    step_configs: &HashMap<String, SkillSteps>,
) -> StepGateEvaluation {
    let mut evaluation = StepGateEvaluation {
        tool: input.tool_name.clone(),
        tool_present: input
            .tool_name
            .as_deref()
            .is_some_and(|name| !name.is_empty()),
        step_tool: false,
        skill: None,
        step_id: None,
        step_config_loaded: false,
        step_declared: false,
        phase_id: None,
        target_step_id: None,
        target_step_description: None,
        prerequisite_present: false,
        prerequisite_step_id: None,
        prerequisite_description: None,
        graph_workflow_present: false,
        prerequisite_graph_completed: false,
        proof_chain_present: false,
        step_proof_present: false,
        should_deny: false,
        decision: StepGateDecision::Allow,
    };

    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return evaluation,
    };

    // Only step tools are step-gated. Everything else falls through.
    let step_ref = match StepToolRef::parse(tool_name) {
        Some(r) => r,
        None => return evaluation,
    };
    evaluation.step_tool = true;
    evaluation.skill = Some(step_ref.skill.clone());
    evaluation.step_id = Some(step_ref.step_id.clone());

    let skill_steps = match step_configs.get(&step_ref.skill) {
        Some(s) => {
            evaluation.step_config_loaded = true;
            s
        }
        None => {
            evaluation.should_deny = true;
            evaluation.decision = StepGateDecision::DenyMissingStepConfig;
            return evaluation;
        }
    };

    let step_plan = match step_plan_ref(skill_steps, &step_ref.step_id) {
        Some(plan) => {
            evaluation.step_declared = true;
            evaluation.phase_id = Some(plan.phase_id.to_string());
            evaluation.target_step_id = Some(plan.target.id.clone());
            evaluation.target_step_description = Some(plan.target.description.clone());
            if let Some(prereq) = plan.prerequisite {
                evaluation.prerequisite_present = true;
                evaluation.prerequisite_step_id = Some(prereq.id.clone());
                evaluation.prerequisite_description = Some(prereq.description.clone());
            }
            plan
        }
        None => {
            evaluation.should_deny = true;
            evaluation.decision = StepGateDecision::DenyStepNotDeclared;
            return evaluation;
        }
    };

    let graph_workflow = match state.graph_workflow(&step_ref.skill) {
        Some(workflow) => {
            evaluation.graph_workflow_present = true;
            workflow
        }
        None => {
            evaluation.should_deny = true;
            evaluation.decision = StepGateDecision::DenyMissingGraphWorkflow;
            return evaluation;
        }
    };

    // First step in a phase has no step prerequisite, but it still requires the
    // graph checkpoint above so step execution is not detached from LangGraph.
    let Some(prereq) = step_plan.prerequisite else {
        evaluation.decision = StepGateDecision::AllowFirstStep;
        return evaluation;
    };

    if !graph_step_completed(graph_workflow, step_plan.phase_id, &prereq.id) {
        evaluation.should_deny = true;
        evaluation.decision = StepGateDecision::DenyPrerequisiteNotCompleted;
        return evaluation;
    }
    evaluation.prerequisite_graph_completed = true;

    // The graph checkpoint is authoritative for workflow position; StepProof
    // remains the cryptographic receipt. Both must agree before the next step
    // tool is allowed.
    let chain = match state.proof_chain(&step_ref.skill) {
        Some(c) => {
            evaluation.proof_chain_present = true;
            c
        }
        None => {
            evaluation.should_deny = true;
            evaluation.decision = StepGateDecision::DenyMissingProofChain;
            return evaluation;
        }
    };

    // The actual gate check: does the prereq step's proof exist?
    if has_step_proof(chain, &step_ref.skill, &prereq.id) {
        evaluation.step_proof_present = true;
        evaluation.decision = StepGateDecision::AllowPrerequisiteProof;
        return evaluation;
    }

    evaluation.should_deny = true;
    evaluation.decision = StepGateDecision::DenyMissingStepProof;
    evaluation
}

#[must_use]
pub fn output_from_evaluation(input: &HookInput, evaluation: &StepGateEvaluation) -> HookOutput {
    let requires_identity = !matches!(
        evaluation.decision,
        StepGateDecision::Allow
            | StepGateDecision::AllowFirstStep
            | StepGateDecision::AllowPrerequisiteProof
    );
    let tool = if requires_identity {
        match required_output_context(input, "tool identity", evaluation.tool.as_deref()) {
            Ok(value) => value,
            Err(output) => return output,
        }
    } else {
        ""
    };
    let skill = if requires_identity {
        match required_output_context(input, "skill identity", evaluation.skill.as_deref()) {
            Ok(value) => value,
            Err(output) => return output,
        }
    } else {
        ""
    };
    let step_id = if matches!(
        evaluation.decision,
        StepGateDecision::DenyStepNotDeclared | StepGateDecision::DenyMissingProofChain
    ) {
        match required_output_context(input, "step identity", evaluation.step_id.as_deref()) {
            Ok(value) => value,
            Err(output) => return output,
        }
    } else {
        ""
    };
    let phase_id = if matches!(
        evaluation.decision,
        StepGateDecision::DenyPrerequisiteNotCompleted
    ) {
        match required_output_context(input, "phase identity", evaluation.phase_id.as_deref()) {
            Ok(value) => value,
            Err(output) => return output,
        }
    } else {
        ""
    };
    let prereq_required = matches!(
        evaluation.decision,
        StepGateDecision::DenyPrerequisiteNotCompleted
            | StepGateDecision::DenyMissingProofChain
            | StepGateDecision::DenyMissingStepProof
    );
    let prereq_id = if prereq_required {
        match required_output_context(
            input,
            "prerequisite step identity",
            evaluation.prerequisite_step_id.as_deref(),
        ) {
            Ok(value) => value,
            Err(output) => return output,
        }
    } else {
        ""
    };
    let prereq_desc = if prereq_required {
        match required_output_context(
            input,
            "prerequisite description",
            evaluation.prerequisite_description.as_deref(),
        ) {
            Ok(value) => value,
            Err(output) => return output,
        }
    } else {
        ""
    };
    match evaluation.decision {
        StepGateDecision::Allow
        | StepGateDecision::AllowFirstStep
        | StepGateDecision::AllowPrerequisiteProof => HookOutput::allow(),
        StepGateDecision::DenyMissingStepConfig => deny_with_context(
            input,
            format!(
                "[Sentinel-Authority] step_gate: refusing tool '{tool}' \
                 for skill '{skill}' — no step config is loaded. Step tools \
                 must be backed by a configured LangGraph/StepProof plan."
            ),
        ),
        StepGateDecision::DenyStepNotDeclared => deny_with_context(
            input,
            format!(
                "[Sentinel-Authority] step_gate: refusing tool '{tool}' \
                 for skill '{skill}' — step '{step_id}' is not declared in \
                 the loaded LangGraph/StepProof plan."
            ),
        ),
        StepGateDecision::DenyMissingGraphWorkflow => deny_with_context(
            input,
            format!(
                "[Sentinel-Authority] step_gate: refusing tool '{tool}' \
                 for skill '{skill}' — no LangGraph checkpoint projection \
                 is active. Run the skill's phase gate first so step \
                 execution is anchored to the durable graph."
            ),
        ),
        StepGateDecision::DenyPrerequisiteNotCompleted => deny_with_context(
            input,
            format!(
                "[Sentinel-Authority] step_gate: refusing tool '{tool}' for skill \
                 '{skill}' — prerequisite step '{prereq_id}' ({prereq_desc}) is \
                 not completed in the active LangGraph checkpoint for phase \
                 '{phase_id}'. Complete that step through \
                 sentinel__submit_step_complete first."
            ),
        ),
        StepGateDecision::DenyMissingProofChain => deny_with_context(
            input,
            format!(
                "[Sentinel-Authority] step_gate: refusing tool '{tool}' \
                 for skill '{skill}' — no active proof chain. The skill's \
                 step '{prereq_id}' ({prereq_desc}) must complete (and \
                 produce a StepProof) before step '{step_id}' can be \
                 invoked."
            ),
        ),
        StepGateDecision::DenyMissingStepProof => deny_with_context(
            input,
            format!(
                "[Sentinel-Authority] step_gate: refusing tool '{tool}' for skill \
                 '{skill}' — prerequisite step '{prereq_id}' ({prereq_desc}) has \
                 no StepProof in the active chain. Complete that step first."
            ),
        ),
    }
}

fn required_output_context<'a>(
    input: &HookInput,
    field: &str,
    value: Option<&'a str>,
) -> Result<&'a str, HookOutput> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            deny_with_context(
                input,
                format!(
                    "[Sentinel-Authority] step_gate: refusing unaudited step decision — \
                     missing concrete {field}."
                ),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::events::PermissionDecision;
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::JudgeVerdict;
    use sentinel_domain::proof::GENESIS_HASH;
    use sentinel_domain::step_proof::StepProof;
    use sentinel_domain::workflow::{PhaseSteps, WorkflowState};

    fn step_input(tool: &str) -> HookInput {
        HookInput {
            tool_name: Some(tool.to_string()),
            ..HookInput::default()
        }
    }

    /// Convenience: pull the (decision, reason) pair from a HookOutput so
    /// tests can assert against the real shape (no `decision_string()` API
    /// exists on HookOutput — assertions read `permission_decision` and
    /// `permission_decision_reason` directly).
    fn decision_of(out: &HookOutput) -> (Option<PermissionDecision>, Option<&str>) {
        let hso = out.hook_specific_output.as_ref();
        let decision = hso.and_then(|h| h.permission_decision);
        let reason = hso.and_then(|h| h.permission_decision_reason.as_deref());
        (decision, reason)
    }

    fn is_allow(out: &HookOutput) -> bool {
        let (decision, _) = decision_of(out);
        // allow() returns Self::default() — no permission_decision set.
        decision.is_none() && !matches!(out.blocked, Some(true))
    }

    fn is_deny(out: &HookOutput) -> bool {
        matches!(decision_of(out).0, Some(PermissionDecision::Deny))
    }

    fn linear_step_config() -> SkillSteps {
        SkillSteps {
            skill: "linear".to_string(),
            federation_version: "1".to_string(),
            phases: vec![PhaseSteps {
                phase_id: "claim".to_string(),
                steps: vec![
                    WorkflowStep {
                        id: "1".to_string(),
                        description: "fetch ticket".to_string(),
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
                        id: "2".to_string(),
                        description: "create branch".to_string(),
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
                        id: "3".to_string(),
                        description: "open PR".to_string(),
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
            }],
        }
    }

    fn make_step_proof(skill: &str, step_id: &str, phase_id: &str) -> StepProof {
        let evidence = Evidence::default();
        let evidence_hash = StepProof::compute_evidence_hash(&evidence);
        let artifact = serde_json::Value::Null;
        let artifact_hash = StepProof::compute_artifact_hash(&artifact);
        let combined_hash = StepProof::compute_combined_hash(
            step_id,
            phase_id,
            skill,
            &evidence_hash,
            &artifact_hash,
            GENESIS_HASH,
            true,
        );
        StepProof {
            step_id: step_id.into(),
            phase_id: phase_id.into(),
            skill: skill.into(),
            session_id: "sess-1".into(),
            evidence,
            evidence_hash,
            artifact,
            artifact_hash,
            account_context: None,
            previous_hash: GENESIS_HASH.into(),
            combined_hash,
            judge_model: "kimi-k2.6".into(),
            judge_verdict: JudgeVerdict {
                sufficient: true,
                confidence: 0.95,
                reasoning: "ok".into(),
                requested_evidence: None,
            },
            signature: None,
            trace_context: None,
            started_at: chrono::Utc::now(),
            completed_at: chrono::Utc::now(),
            duration_ms: 5,
        }
    }

    fn activate_graph_state(state: &mut SessionState, skill: &str) {
        let mut workflow = WorkflowState::new(skill, "sess-1");
        workflow.current_phase = Some(0);
        state.set_graph_projected_workflow(skill.to_string(), workflow);
    }

    fn complete_graph_step(state: &mut SessionState, skill: &str, phase_id: &str, step_id: &str) {
        let mut workflow = WorkflowState::new(skill, "sess-1");
        workflow.current_phase = Some(0);
        workflow.update_step(
            phase_id,
            step_id,
            StepStatus::Completed,
            Some("done".to_string()),
        );
        state.set_graph_projected_workflow(skill.to_string(), workflow);
    }

    #[test]
    fn parse_valid_step_tool_name() {
        let r = StepToolRef::parse("mcp__skills__linear__step_3").expect("parses");
        assert_eq!(r.skill, "linear");
        assert_eq!(r.step_id, "3");
    }

    #[test]
    fn parse_step_id_with_dots_and_underscores() {
        // Compound step IDs like "3.L2.3" or "claim_init" must round-trip.
        let r = StepToolRef::parse("mcp__skills__linear__step_3.L2.3").expect("parses");
        assert_eq!(r.skill, "linear");
        assert_eq!(r.step_id, "3.L2.3");

        let r = StepToolRef::parse("mcp__skills__git__step_create_branch").expect("parses");
        assert_eq!(r.skill, "git");
        assert_eq!(r.step_id, "create_branch");
    }

    #[test]
    fn parse_rejects_non_step_tools() {
        assert!(StepToolRef::parse("Read").is_none());
        assert!(StepToolRef::parse("Bash").is_none());
        assert!(StepToolRef::parse("mcp__linear__create_issue").is_none());
        assert!(StepToolRef::parse("mcp__skills__linear").is_none());
        assert!(StepToolRef::parse("mcp__skills____step_1").is_none()); // empty skill
        assert!(StepToolRef::parse("mcp__skills__linear__step_").is_none()); // empty step_id
    }

    #[test]
    fn allow_when_no_tool_name() {
        let mut input = step_input("ignored");
        input.tool_name = None;
        let state = SessionState::new("sess-1");
        let configs = HashMap::new();
        assert!(is_allow(&process(&input, &state, &configs)));
    }

    #[test]
    fn allow_non_step_tools() {
        let state = SessionState::new("sess-1");
        let configs = HashMap::new();
        for tool in ["Read", "Bash", "Edit", "mcp__linear__create_issue"] {
            assert!(
                is_allow(&process(&step_input(tool), &state, &configs)),
                "tool {tool} should pass through step_gate",
            );
        }
    }

    #[test]
    fn deny_skills_without_step_config() {
        let state = SessionState::new("sess-1");
        let configs = HashMap::new(); // empty — no skill registered
        let out = process(&step_input("mcp__skills__deploy__step_1"), &state, &configs);
        assert!(is_deny(&out), "expected deny, got {out:?}");
        let reason = decision_of(&out).1.expect("deny reason");
        assert!(
            reason.contains("no step config is loaded"),
            "deny reason must identify missing step config: {reason}"
        );
    }

    #[test]
    fn deny_first_step_without_langgraph_checkpoint() {
        let state = SessionState::new("sess-1");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(&step_input("mcp__skills__linear__step_1"), &state, &configs);
        assert!(is_deny(&out));
        let reason = decision_of(&out).1.expect("deny reason");
        assert!(
            reason.contains("no LangGraph checkpoint projection"),
            "deny reason must require graph checkpoint: {reason}"
        );
    }

    #[test]
    fn allow_first_step_in_phase_with_langgraph_checkpoint() {
        // Step "1" has no step prereq within "claim" phase, but step tools
        // still require an active graph checkpoint projection.
        let mut state = SessionState::new("sess-1");
        activate_graph_state(&mut state, "linear");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(&step_input("mcp__skills__linear__step_1"), &state, &configs);
        assert!(is_allow(&out));
    }

    #[test]
    fn deny_unknown_configured_skill_step() {
        let mut state = SessionState::new("sess-1");
        activate_graph_state(&mut state, "linear");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(
            &step_input("mcp__skills__linear__step_999"),
            &state,
            &configs,
        );
        assert!(is_deny(&out));
        let reason = decision_of(&out).1.expect("deny reason");
        assert!(
            reason.contains("is not declared"),
            "deny reason must identify undeclared step: {reason}"
        );
    }

    #[test]
    fn deny_when_no_active_chain_for_non_first_step() {
        // step_2 has step_1 as prereq, but session has no chain at all.
        let mut state = SessionState::new("sess-1");
        activate_graph_state(&mut state, "linear");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_deny(&out), "expected deny, got {out:?}");
    }

    #[test]
    fn deny_when_prereq_step_proof_missing() {
        // Chain exists but doesn't contain step_1's proof.
        let mut state = SessionState::new("sess-1");
        complete_graph_step(&mut state, "linear", "claim", "1");
        let chain = ProofChain::new("linear", "sess-1");
        state.restore_proof_chain("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_deny(&out));
    }

    #[test]
    fn allow_when_prereq_step_proof_exists() {
        // Chain contains step_1's proof => step_2 should pass.
        let mut state = SessionState::new("sess-1");
        complete_graph_step(&mut state, "linear", "claim", "1");
        let mut chain = ProofChain::new("linear", "sess-1");
        let proof = make_step_proof("linear", "1", "claim");
        chain
            .add_step_proof(proof)
            .expect("step_1 proof appends to fresh chain");
        state.restore_proof_chain("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_allow(&out));
    }

    #[test]
    fn step_3_blocked_until_step_2_proof_exists() {
        // Chain has step_1 but not step_2 — step_3 must still be denied.
        let mut state = SessionState::new("sess-1");
        complete_graph_step(&mut state, "linear", "claim", "1");
        let mut chain = ProofChain::new("linear", "sess-1");
        chain
            .add_step_proof(make_step_proof("linear", "1", "claim"))
            .expect("step_1");
        state.restore_proof_chain("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_3"), &state, &configs);
        assert!(is_deny(&out), "step_3 must wait on step_2's proof");
    }

    #[test]
    fn deny_when_proof_exists_but_langgraph_prereq_is_not_complete() {
        let mut state = SessionState::new("sess-1");
        activate_graph_state(&mut state, "linear");
        let mut chain = ProofChain::new("linear", "sess-1");
        chain
            .add_step_proof(make_step_proof("linear", "1", "claim"))
            .expect("step_1");
        state.restore_proof_chain("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_deny(&out), "stale proof without graph state must deny");
        let reason = decision_of(&out).1.expect("deny reason");
        assert!(
            reason.contains("active LangGraph checkpoint"),
            "deny reason must cite graph checkpoint state: {reason}"
        );
    }

    #[test]
    fn deny_message_names_the_missing_step() {
        let mut state = SessionState::new("sess-1");
        activate_graph_state(&mut state, "linear");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        let (_, reason) = decision_of(&out);
        let msg = reason.expect("deny carries a permission_decision_reason");
        // Message must include the prereq step id so the model knows what to fix.
        assert!(
            msg.contains('1'),
            "deny message should name prereq id '1', got: {msg}"
        );
        // Provenance prefix tells Claude this came from sentinel.
        assert!(
            msg.contains("[Sentinel-Authority]"),
            "deny message should carry provenance prefix, got: {msg}",
        );
    }

    #[test]
    fn deny_output_requires_concrete_graph_identity() {
        let evaluation = StepGateEvaluation {
            tool: None,
            tool_present: false,
            step_tool: true,
            skill: Some("linear".to_string()),
            step_id: Some("2".to_string()),
            step_config_loaded: false,
            step_declared: false,
            phase_id: None,
            target_step_id: None,
            target_step_description: None,
            prerequisite_present: false,
            prerequisite_step_id: None,
            prerequisite_description: None,
            graph_workflow_present: false,
            prerequisite_graph_completed: false,
            proof_chain_present: false,
            step_proof_present: false,
            should_deny: true,
            decision: StepGateDecision::DenyMissingStepConfig,
        };

        let out = output_from_evaluation(&HookInput::default(), &evaluation);
        assert!(is_deny(&out));
        let reason = decision_of(&out).1.expect("deny reason");
        assert!(
            reason.contains("missing concrete tool identity"),
            "malformed graph-required output must fail closed: {reason}"
        );
        assert!(
            !reason.contains("unknown"),
            "deny reason must not synthesize fallback identity: {reason}"
        );
    }
}
