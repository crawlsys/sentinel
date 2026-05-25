//! Step Gate Hook
//!
//! PreToolUse hook that blocks fine-grained step tool calls when their
//! prerequisite step has not been completed (i.e. no [`StepProof`] exists in
//! the active [`ProofChain`] for the prior step).
//!
//! # Relationship to `phase_gate`
//!
//! `phase_gate` enforces *coarse* phase boundaries: did you read the phase
//! file before invoking write/exec tools? `step_gate` layers underneath at
//! finer granularity: within a phase, did the prior step in the configured
//! sequence produce a proof before the next step's tool is called?
//!
//! Both hooks fire on PreToolUse. They are **complementary**, not redundant:
//! - Skills with no step config fall through `step_gate` (no-op) and rely on
//!   `phase_gate` alone — backwards compatible with the existing 76 skills.
//! - Skills *with* step configs get both layers — phase_gate prevents whole
//!   phases from being skipped; step_gate prevents steps within a phase
//!   from being skipped.
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
//!    (M2.5 federation directives — for now, "the previous step in declared
//!    order within the same phase").
//! 2. Looks up the active `ProofChain` for the skill in `SessionState`.
//! 3. Verifies a `StepProof` exists for the prerequisite step.
//! 4. Blocks with a `[Sentinel-Authority]` deny message naming the missing
//!    prerequisite if not.
//!
//! # Glass break + sentinel authority
//!
//! Same glass-break semantics as `phase_gate`: an active break bypasses
//! enforcement (with the tool call logged for audit). Block messages carry
//! the `[Sentinel-Authority]` provenance prefix consumed by Claude Code.

use std::collections::HashMap;

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::proof::{ProofChain, ProofEntry};
use sentinel_domain::state::SessionState;
use sentinel_domain::workflow::{SkillSteps, WorkflowStep};

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

/// Find the prerequisite step for a target step within a skill's step config.
///
/// Strategy (interim, until M2.5 federation directives land): the prereq is
/// the immediately-preceding step *in declared order within the same phase*.
/// First step in a phase has no prereq (returns `None`).
///
/// When M2.5 ships, this becomes "consult `requires:` field on the step
/// config" instead of positional ordering — but the hook signature stays
/// the same.
fn prerequisite_step<'a>(
    skill_steps: &'a SkillSteps,
    target_step_id: &str,
) -> Option<&'a WorkflowStep> {
    for phase in &skill_steps.phases {
        let mut prev: Option<&WorkflowStep> = None;
        for step in &phase.steps {
            if step.id == target_step_id {
                return prev;
            }
            prev = Some(step);
        }
    }
    None
}

/// Check whether a `StepProof` exists in `chain` for the given
/// `(skill, step_id)` pair.
///
/// Walks the mixed-entry chain (`entries`) — phase-only chains never
/// contain step proofs, so the legacy `proofs` Vec is intentionally
/// ignored here.
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

/// Process a step-gate hook event (PreToolUse).
///
/// Returns:
/// - [`HookOutput::allow`] for tool calls that aren't step tools, for skills
///   without step configs (backwards compat), for the first step in a phase
///   (no prereq), and for steps whose prereq proof exists.
/// - [`HookOutput::deny`] with a `[Sentinel-Authority]` message when the
///   prereq StepProof is missing.
pub fn process(
    input: &HookInput,
    state: &SessionState,
    step_configs: &HashMap<String, SkillSteps>,
) -> HookOutput {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return HookOutput::allow(),
    };

    // ── Glass break emergency override (same semantics as phase_gate) ─────
    // We only *read* state here; clearing expired breaks is phase_gate's job
    // since both hooks share state and clearing twice is idempotent.
    if state.is_break_active() {
        return HookOutput::allow();
    }

    // Only step tools are step-gated. Everything else falls through.
    let step_ref = match StepToolRef::parse(tool_name) {
        Some(r) => r,
        None => return HookOutput::allow(),
    };

    // Skills without step config registered are phase-gated only — let
    // phase_gate handle them. This is the backwards-compat path for the 76
    // skills that haven't been migrated to step-level enforcement.
    let skill_steps = match step_configs.get(&step_ref.skill) {
        Some(s) => s,
        None => return HookOutput::allow(),
    };

    // Identify the prerequisite step. First step in a phase has no prereq.
    let prereq = match prerequisite_step(skill_steps, &step_ref.step_id) {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    // Find the active proof chain for this skill. No chain yet => the skill
    // hasn't started; first step is allowed (its prereq lookup returned
    // None above), and any non-first step needs a chain to even compare
    // against, so we deny with a clear message.
    let chain = match state.proof_chains.get(&step_ref.skill) {
        Some(c) => c,
        None => {
            return deny_with_context(
                input,
                format!(
                    "[Sentinel-Authority] step_gate: refusing tool '{tool}' \
                     for skill '{skill}' — no active proof chain. The skill's \
                     step '{prereq_id}' ({prereq_desc}) must complete (and \
                     produce a StepProof) before step '{step_id}' can be \
                     invoked.",
                    tool = tool_name,
                    skill = step_ref.skill,
                    prereq_id = prereq.id,
                    prereq_desc = prereq.description,
                    step_id = step_ref.step_id,
                ),
            );
        }
    };

    // The actual gate check: does the prereq step's proof exist?
    if has_step_proof(chain, &step_ref.skill, &prereq.id) {
        return HookOutput::allow();
    }

    deny_with_context(
        input,
        format!(
            "[Sentinel-Authority] step_gate: refusing tool '{tool}' for skill \
             '{skill}' — prerequisite step '{prereq_id}' ({prereq_desc}) has \
             no StepProof in the active chain. Complete that step first.",
            tool = tool_name,
            skill = step_ref.skill,
            prereq_id = prereq.id,
            prereq_desc = prereq.description,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::events::PermissionDecision;
    use sentinel_domain::evidence::Evidence;
    use sentinel_domain::judge::JudgeVerdict;
    use sentinel_domain::proof::GENESIS_HASH;
    use sentinel_domain::step_proof::StepProof;
    use sentinel_domain::workflow::PhaseSteps;

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
        // allow() returns Self::default() — no permission_decision set, no
        // legacy `blocked` flag.
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
            actor: None,
        }
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
    fn allow_skills_without_step_config() {
        // Backwards compat: a step tool for a skill that has no step config
        // registered must fall through to phase_gate, not deny.
        let state = SessionState::new("sess-1");
        let configs = HashMap::new(); // empty — no skill registered
        let out = process(&step_input("mcp__skills__deploy__step_1"), &state, &configs);
        assert!(is_allow(&out));
    }

    #[test]
    fn allow_first_step_in_phase() {
        // Step "1" has no prereq within "claim" phase — first step is always
        // allowed regardless of chain state.
        let state = SessionState::new("sess-1");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(&step_input("mcp__skills__linear__step_1"), &state, &configs);
        assert!(is_allow(&out));
    }

    #[test]
    fn deny_when_no_active_chain_for_non_first_step() {
        // step_2 has step_1 as prereq, but session has no chain at all.
        let state = SessionState::new("sess-1");
        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());
        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_deny(&out), "expected deny, got {out:?}");
    }

    #[test]
    fn deny_when_prereq_step_proof_missing() {
        // Chain exists but doesn't contain step_1's proof.
        let mut state = SessionState::new("sess-1");
        let chain = ProofChain::new("linear", "sess-1");
        state.proof_chains.insert("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_deny(&out));
    }

    #[test]
    fn allow_when_prereq_step_proof_exists() {
        // Chain contains step_1's proof => step_2 should pass.
        let mut state = SessionState::new("sess-1");
        let mut chain = ProofChain::new("linear", "sess-1");
        let proof = make_step_proof("linear", "1", "claim");
        chain
            .add_step_proof(proof)
            .expect("step_1 proof appends to fresh chain");
        state.proof_chains.insert("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_2"), &state, &configs);
        assert!(is_allow(&out));
    }

    #[test]
    fn step_3_blocked_until_step_2_proof_exists() {
        // Chain has step_1 but not step_2 — step_3 must still be denied.
        let mut state = SessionState::new("sess-1");
        let mut chain = ProofChain::new("linear", "sess-1");
        chain
            .add_step_proof(make_step_proof("linear", "1", "claim"))
            .expect("step_1");
        state.proof_chains.insert("linear".to_string(), chain);

        let mut configs = HashMap::new();
        configs.insert("linear".to_string(), linear_step_config());

        let out = process(&step_input("mcp__skills__linear__step_3"), &state, &configs);
        assert!(is_deny(&out), "step_3 must wait on step_2's proof");
    }

    #[test]
    fn deny_message_names_the_missing_step() {
        let state = SessionState::new("sess-1");
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
}
