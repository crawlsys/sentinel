//! Step Judge Hook
//!
//! `PostToolUse` hook that runs the AI judge against a completed step's
//! evidence and prepares the verdict for `submit_step_complete` (M1.5) to
//! seal into the proof chain.
//!
//! # Position in the loop
//!
//! ```text
//! agent calls mcp__skills__<skill>__step_<n>
//!     │
//!     ▼
//! step_gate (M1.3) — checks prereq StepProof exists, allows or denies
//!     │  (allow path)
//!     ▼
//! tool executes, returns result captured by sentinel
//!     │
//!     ▼
//! step_judge (M1.4) — gather evidence, call JudgeService, produce verdict
//!     │
//!     ▼
//! submit_step_complete (M1.5) — build StepProof, append to chain
//!     │
//!     ▼
//! next step's step_gate sees the proof, allows the next tool call
//! ```
//!
//! # What this hook does
//!
//! 1. Detects `PostToolUse` for a step tool (same `mcp__skills__<skill>__step_<id>`
//!    naming as `step_gate`).
//! 2. Resolves the step's description from the loaded step config (so the
//!    judge prompt knows what "sufficient" looks like for this specific step).
//! 3. Gathers a step-shaped [`Evidence`] from the tool input/output.
//! 4. Invokes the [`JudgeService`] trait.
//! 5. Returns the [`JudgeVerdict`] inside a [`StepJudgeOutcome`] struct that
//!    M1.5's `submit_step_complete` will consume.
//!
//! Persistence (writing the `StepProof` to disk, advancing the chain head)
//! lives in M1.5 — this hook is the *verdict* layer; the *write* layer
//! comes next.
//!
//! # Cross-vendor judge support (#73)
//!
//! `JudgeModel` is plumbed through to `JudgeService::evaluate_step` so
//! per-step config can request `JudgeModel::Sonnet` (cheap routine),
//! `JudgeModel::Opus` (deep critical), or future enum variants for
//! OpenRouter-routed Kimi/Codex/etc. The hook itself stays model-agnostic;
//! it picks the right tier from the step config and lets the trait route.
//!
//! # Sentinel authority
//!
//! The hook never blocks via `HookOutput::deny` — failed verdicts surface
//! as a non-sufficient `JudgeVerdict` that `submit_step_complete` will
//! refuse to seal. `PostToolUse` blocking is the wrong layer for "this step
//! didn't pass"; the chain itself is the enforcement substrate, not this
//! hook's allow/deny return.

use std::collections::HashMap;

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::evidence::{Evidence, ToolCallEvidence, ToolResultEvidence};
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
use sentinel_domain::state::{BaselineCounter, SessionState};
use sentinel_domain::workflow::SkillSteps;

use crate::judge_service::JudgeService;

/// Parsed components of a `mcp__skills__<skill>__step_<step_id>` tool name.
/// Mirrors the parser in `step_gate` so both hooks recognize the same
/// federated namespace produced by skills-mcp (M2).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepToolRef {
    skill: String,
    step_id: String,
}

impl StepToolRef {
    fn parse(tool_name: &str) -> Option<Self> {
        let rest = tool_name.strip_prefix("mcp__skills__")?;
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

/// Result of `step_judge` evaluation. Consumed by M1.5's
/// `submit_step_complete` to build the next [`StepProof`] in the chain.
#[derive(Debug, Clone)]
pub enum StepJudgeOutcome {
    /// Hook fired but the tool wasn't a step tool — no verdict, no-op.
    NotAStepTool,
    /// Skill has no step config registered. Step tools must have a loaded
    /// LangGraph/StepProof plan.
    MissingStepConfig { skill: String },
    /// Skill has a step config but the specific `step_id` wasn't found in any
    /// phase. Possible misconfig or stale tool name.
    UnknownStep { skill: String, step_id: String },
    /// Judge ran, here's the verdict + the resolved coordinates so M1.5 can
    /// build the `StepProof` without reparsing the tool name.
    Judged {
        skill: String,
        phase_id: String,
        step_id: String,
        step_description: String,
        evidence: Evidence,
        verdict: JudgeVerdict,
        judge_model: JudgeModel,
    },
    /// **Cold-start baseline (M1.8)**: judge ran but the step is still in
    /// its warmup window. Verdict is observational — `submit_step_complete`
    /// must NOT seal a `StepProof` for this outcome (the chain only carries
    /// enforced verdicts). The verdict is still surfaced for telemetry so
    /// operators can watch warmup-time false-positive rates and decide
    /// when to lower the threshold.
    JudgedWarmup {
        skill: String,
        phase_id: String,
        step_id: String,
        step_description: String,
        evidence: Evidence,
        verdict: JudgeVerdict,
        judge_model: JudgeModel,
        baseline: BaselineCounter,
        threshold: u64,
    },
    /// Judge call returned an error (network, missing API keys, parse
    /// failure). The chain is NOT extended — `submit_step_complete` only
    /// runs on `Judged` outcomes.
    JudgeError {
        skill: String,
        step_id: String,
        error: String,
    },
}

/// Resolve `(phase_id, step_description, baseline_threshold)` for a given
/// `step_id` within a skill's step config. Returns `None` if the step
/// isn't found in any phase.
fn locate_step<'a>(
    skill_steps: &'a SkillSteps,
    step_id: &str,
) -> Option<(&'a str, &'a str, u64, Option<JudgeModel>)> {
    for phase in &skill_steps.phases {
        for step in &phase.steps {
            if step.id == step_id {
                return Some((
                    phase.phase_id.as_str(),
                    step.description.as_str(),
                    step.baseline_threshold,
                    step.judge,
                ));
            }
        }
    }
    None
}

/// Truncate a JSON value to its first ~500 chars when serialized — matches
/// the convention used by the existing `ToolResultEvidence::result_summary`
/// field, which deliberately caps content to keep evidence hashing cheap.
fn truncate_json_summary(v: &serde_json::Value, max: usize) -> String {
    let s = v.to_string();
    if s.len() <= max {
        s
    } else {
        // Char-boundary safe truncation — JSON is ASCII-heavy but quoted
        // strings can hold UTF-8, so we slice on a valid boundary.
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

/// Build a step-shaped Evidence blob from `PostToolUse` `HookInput`.
///
/// The judge will see:
/// - one `ToolCallEvidence` entry summarising the step tool's input
/// - one `ToolResultEvidence` entry summarising the step tool's output
/// - the original `tool_input` / `tool_result` JSON in `custom` so judges
///   that want full payloads can read past the 500-char summaries
/// - `phase_file_read = true` as a proxy for "we have real evidence here"
///
/// Richer evidence sources (file hashes, exit codes, external receipts
/// from #77 the bible, Browserbase recordings from #74) layer on top in
/// M1.5+ via `EvidenceSource` adapters — those will mutate `custom` /
/// `tool_results` before the judge call.
fn gather_evidence(input: &HookInput) -> Evidence {
    let mut evidence = Evidence::default();
    evidence.phase_file_read = true;

    let tool_name = input.tool_name.as_deref().unwrap_or("unknown").to_string();
    let timestamp = chrono::Utc::now().to_rfc3339();

    if let Some(tool_input) = input.tool_input.as_ref() {
        evidence.tool_calls.push(ToolCallEvidence {
            tool: tool_name.clone(),
            args_summary: truncate_json_summary(tool_input, 500),
            timestamp,
        });
    }

    if let Some(tool_result) = input.tool_result.as_ref() {
        // Heuristic for "did the tool succeed": presence of an error field
        // or a top-level boolean false ⇒ failure. Otherwise assume success.
        // Real M1.5 evidence work will replace this with structured exit
        // codes from sentinel's tool-result capture path.
        let success = tool_result.get("error").is_none() && tool_result.as_bool().is_none_or(|b| b);
        evidence.tool_results.push(ToolResultEvidence {
            tool: tool_name.clone(),
            result_summary: truncate_json_summary(tool_result, 500),
            success,
        });
    }

    // Stash the full payloads in `custom` for judges that want detail
    // beyond the 500-char summaries. Keyed under "step_tool_io" so future
    // hooks know to look here.
    let mut custom = serde_json::Map::new();
    custom.insert(
        "step_tool_name".into(),
        serde_json::Value::String(tool_name),
    );
    if let Some(tool_input) = input.tool_input.clone() {
        custom.insert("step_tool_input".into(), tool_input);
    }
    if let Some(tool_result) = input.tool_result.clone() {
        custom.insert("step_tool_result".into(), tool_result);
    }
    evidence.custom = serde_json::Value::Object(custom);

    evidence
}

/// Process a step-judge hook event (`PostToolUse`).
///
/// Returns `(HookOutput, StepJudgeOutcome)`. The `HookOutput` is always
/// `allow` for this hook — we don't block tool calls in `PostToolUse`, we
/// just produce verdicts. The `StepJudgeOutcome` carries the verdict (or
/// the reason no verdict was produced) up to the caller; M1.5's wiring
/// in `hook_cmd.rs` will consume it to call `submit_step_complete` when
/// `Outcome::Judged`.
pub async fn process(
    input: &HookInput,
    state: &mut SessionState,
    step_configs: &HashMap<String, SkillSteps>,
    judge: &dyn JudgeService,
) -> (HookOutput, StepJudgeOutcome) {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return (HookOutput::allow(), StepJudgeOutcome::NotAStepTool),
    };

    let step_ref = match StepToolRef::parse(tool_name) {
        Some(r) => r,
        None => return (HookOutput::allow(), StepJudgeOutcome::NotAStepTool),
    };

    let skill_steps = match step_configs.get(&step_ref.skill) {
        Some(s) => s,
        None => {
            let message = format!(
                "[Sentinel-Authority] step_judge: no verdict for skill '{}' \
                 step '{}' — no step config is loaded. Step tools must be \
                 backed by a configured LangGraph/StepProof plan; \
                 submit_step_complete will not seal this step.",
                step_ref.skill, step_ref.step_id,
            );
            return (
                HookOutput::inject_context(HookEvent::PostToolUse, message),
                StepJudgeOutcome::MissingStepConfig {
                    skill: step_ref.skill,
                },
            );
        }
    };

    let (phase_id, step_description, baseline_threshold, step_judge_model) =
        match locate_step(skill_steps, &step_ref.step_id) {
            Some((p, d, t, j)) => (p.to_string(), d.to_string(), t, j),
            None => {
                let message = format!(
                    "[Sentinel-Authority] step_judge: no verdict for skill '{}' \
                     step '{}' — the step is not declared in the loaded \
                     LangGraph/StepProof plan; submit_step_complete will not \
                     seal this step.",
                    step_ref.skill, step_ref.step_id,
                );
                return (
                    HookOutput::inject_context(HookEvent::PostToolUse, message),
                    StepJudgeOutcome::UnknownStep {
                        skill: step_ref.skill,
                        step_id: step_ref.step_id,
                    },
                );
            }
        };

    let evidence = gather_evidence(input);

    // Judge model selection (#73): the step's `judge:` config field if set,
    // else the balanced `Sonnet` tier (claude-sonnet-4.6, $3/$15). A
    // routine step that doesn't pin a `judge:` uses Sonnet; cheap bulk can
    // opt to `kimi`, and a critical step opts up to `opus` explicitly.
    let model = step_judge_model.unwrap_or(JudgeModel::Sonnet);

    let result = judge
        .evaluate_step(
            &step_ref.skill,
            &phase_id,
            &step_ref.step_id,
            &step_description,
            &evidence,
            model,
        )
        .await;

    match result {
        Ok(verdict) => {
            let verdict = verdict.sanitized();

            // **Cold-start baseline (M1.8)**: snapshot the *pre-update*
            // counter so we decide warmup vs enforced based on what the
            // step had cleared BEFORE this judgement. Otherwise the very
            // judgement that crosses the threshold would also be the
            // first one enforced — we want one clean post-threshold
            // judgement before enforcement engages.
            let pre_baseline = state.step_baseline(&step_ref.skill, &phase_id, &step_ref.step_id);
            let in_warmup = !pre_baseline.cleared(baseline_threshold);

            // Record the judgement (both passing and insufficient) so
            // telemetry shows the warmup-time false-positive rate.
            let updated = state.record_step_judgement(
                &step_ref.skill,
                &phase_id,
                &step_ref.step_id,
                verdict.sufficient,
            );

            // Persist the INDEPENDENT verdict (#12 — close the self-certify
            // gap). `submit_step_complete` reads this from the same per-session
            // state so the judge's own verdict — not the caller-supplied one —
            // gates the seal in warn/enforce mode. An agent can no longer
            // grade its own homework by passing `verdict: sufficient=true`.
            state.record_independent_verdict(
                &step_ref.skill,
                &phase_id,
                &step_ref.step_id,
                verdict.sufficient,
                verdict.confidence,
            );

            if in_warmup {
                (
                    HookOutput::allow(),
                    StepJudgeOutcome::JudgedWarmup {
                        skill: step_ref.skill,
                        phase_id,
                        step_id: step_ref.step_id,
                        step_description,
                        evidence,
                        verdict,
                        judge_model: model,
                        baseline: updated,
                        threshold: baseline_threshold,
                    },
                )
            } else {
                (
                    HookOutput::allow(),
                    StepJudgeOutcome::Judged {
                        skill: step_ref.skill,
                        phase_id,
                        step_id: step_ref.step_id,
                        step_description,
                        evidence,
                        verdict,
                        judge_model: model,
                    },
                )
            }
        }
        Err(e) => (
            HookOutput::inject_context(
                HookEvent::PostToolUse,
                format!(
                    "[Sentinel-Authority] step_judge: judge failed for skill '{}' \
                     step '{}' — submit_step_complete will not seal this step. \
                     Error: {e:#}",
                    step_ref.skill, step_ref.step_id,
                ),
            ),
            StepJudgeOutcome::JudgeError {
                skill: step_ref.skill,
                step_id: step_ref.step_id,
                error: format!("{e:#}"),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use sentinel_domain::workflow::{PhaseSteps, WorkflowStep};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Test double: records every evaluate_step call and returns a canned
    /// verdict. Lets us assert the hook routes correctly without a real
    /// LLM.
    struct RecordingJudge {
        calls: AtomicUsize,
        verdict: JudgeVerdict,
    }

    impl RecordingJudge {
        fn passing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                verdict: JudgeVerdict::pass(0.91, "looks good"),
            }
        }

        fn failing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                verdict: JudgeVerdict::fail(0.7, "missing test", vec!["unit tests".into()]),
            }
        }
    }

    #[async_trait::async_trait]
    impl JudgeService for RecordingJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.verdict.clone())
        }
        // Intentionally NOT overriding evaluate_step — exercises the trait's
        // shared implementation that delegates to evaluate.
    }

    struct ErroringJudge;

    #[async_trait::async_trait]
    impl JudgeService for ErroringJudge {
        async fn evaluate(
            &self,
            _skill: &str,
            _phase_id: &str,
            _phase_objectives: &str,
            _evidence: &Evidence,
            _model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            anyhow::bail!("simulated upstream failure")
        }
    }

    fn step_input(tool: &str) -> HookInput {
        HookInput {
            tool_name: Some(tool.into()),
            tool_input: Some(serde_json::json!({"ticket": "FPCRM-1"})),
            tool_result: Some(serde_json::json!({"ok": true, "branch": "fpcrm-1-fix"})),
            ..HookInput::default()
        }
    }

    /// Fresh test state — empty session, no baselines yet. Each test
    /// gets its own independent state so M1.8's per-step counters
    /// don't bleed across tests.
    fn fresh_state() -> SessionState {
        SessionState::new("test-step-judge")
    }

    fn linear_step_config() -> HashMap<String, SkillSteps> {
        let mut m = HashMap::new();
        m.insert(
            "linear".to_string(),
            SkillSteps {
                skill: "linear".into(),
                federation_version: "1".into(),
                phases: vec![PhaseSteps {
                    phase_id: "claim".into(),
                    steps: vec![WorkflowStep {
                        id: "1".into(),
                        description: "Open PR with Ref FPCRM-XXX".into(),
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
                    }],
                }],
            },
        );
        m
    }

    #[tokio::test]
    async fn passes_through_non_step_tools() {
        let judge = RecordingJudge::passing();
        let mut state = fresh_state();
        let (out, outcome) = process(
            &step_input("Read"),
            &mut state,
            &linear_step_config(),
            &judge,
        )
        .await;
        assert!(matches!(outcome, StepJudgeOutcome::NotAStepTool));
        assert!(out.hook_specific_output.is_none());
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            0,
            "judge must NOT fire on non-step tools"
        );
    }

    #[tokio::test]
    async fn no_step_config_for_skill_surfaces_authority_context() {
        let judge = RecordingJudge::passing();
        let configs = HashMap::new(); // empty — no skill registered
        let mut state = fresh_state();
        let (out, outcome) = process(
            &step_input("mcp__skills__deploy__step_1"),
            &mut state,
            &configs,
            &judge,
        )
        .await;
        assert!(matches!(
            outcome,
            StepJudgeOutcome::MissingStepConfig { skill } if skill == "deploy"
        ));
        let context = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("missing step config must inject authority context");
        assert!(
            context.contains("no step config is loaded")
                && context.contains("submit_step_complete will not seal"),
            "unexpected context: {context}"
        );
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_step_id_in_known_skill_reports_error_outcome() {
        let judge = RecordingJudge::passing();
        let mut state = fresh_state();
        let (out, outcome) = process(
            &step_input("mcp__skills__linear__step_99"),
            &mut state,
            &linear_step_config(),
            &judge,
        )
        .await;
        match outcome {
            StepJudgeOutcome::UnknownStep { skill, step_id } => {
                assert_eq!(skill, "linear");
                assert_eq!(step_id, "99");
            }
            other => panic!("expected UnknownStep, got {other:?}"),
        }
        let context = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("unknown step must inject authority context");
        assert!(
            context.contains("not declared in the loaded LangGraph/StepProof plan")
                && context.contains("submit_step_complete will not"),
            "unexpected context: {context}"
        );
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn passing_step_produces_judged_outcome_with_verdict() {
        let judge = RecordingJudge::passing();
        let mut state = fresh_state();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &linear_step_config(),
            &judge,
        )
        .await;
        match outcome {
            StepJudgeOutcome::Judged {
                skill,
                phase_id,
                step_id,
                step_description,
                verdict,
                judge_model,
                ..
            } => {
                assert_eq!(skill, "linear");
                assert_eq!(phase_id, "claim");
                assert_eq!(step_id, "1");
                assert_eq!(step_description, "Open PR with Ref FPCRM-XXX");
                assert!(verdict.sufficient);
                assert!(verdict.confidence > 0.9);
                assert_eq!(judge_model, JudgeModel::Sonnet);
            }
            other => panic!("expected Judged, got {other:?}"),
        }
        assert_eq!(
            judge.calls.load(Ordering::SeqCst),
            1,
            "judge fires exactly once per step"
        );
    }

    #[tokio::test]
    async fn failing_verdict_still_returns_judged_outcome() {
        // Insufficient verdicts do NOT cause an error or skip — the chain
        // is the enforcement layer; submit_step_complete refuses to seal
        // the StepProof when verdict.sufficient == false. step_judge's job
        // is just to produce the verdict.
        let judge = RecordingJudge::failing();
        let mut state = fresh_state();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &linear_step_config(),
            &judge,
        )
        .await;
        match outcome {
            StepJudgeOutcome::Judged { verdict, .. } => {
                assert!(!verdict.sufficient);
                assert!(verdict.requested_evidence.is_some());
            }
            other => panic!("expected Judged with failing verdict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn judge_call_failure_surfaces_as_judge_error() {
        let judge = ErroringJudge;
        let mut state = fresh_state();
        let (out, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &linear_step_config(),
            &judge,
        )
        .await;
        match outcome {
            StepJudgeOutcome::JudgeError {
                skill,
                step_id,
                error,
            } => {
                assert_eq!(skill, "linear");
                assert_eq!(step_id, "1");
                assert!(error.contains("simulated upstream failure"), "got: {error}");
            }
            other => panic!("expected JudgeError, got {other:?}"),
        }
        let context = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("judge failure must inject authority context");
        assert!(
            context.contains("judge failed")
                && context.contains("submit_step_complete will not seal"),
            "unexpected context: {context}"
        );
    }

    // ── M1.8 cold-start baseline tests ─────────────────────────────────

    /// Build a step config with the given baseline_threshold.
    fn config_with_threshold(threshold: u64) -> HashMap<String, SkillSteps> {
        let mut m = HashMap::new();
        m.insert(
            "linear".to_string(),
            SkillSteps {
                skill: "linear".into(),
                federation_version: "1".into(),
                phases: vec![PhaseSteps {
                    phase_id: "claim".into(),
                    steps: vec![WorkflowStep {
                        id: "1".into(),
                        description: "Open PR with Ref FPCRM-XXX".into(),
                        blocker: false,
                        baseline_threshold: threshold,
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
                    }],
                }],
            },
        );
        m
    }

    #[tokio::test]
    async fn warmup_outcome_when_baseline_not_yet_cleared() {
        // Threshold = 3, fresh state with zero successes => first run
        // is in warmup. The verdict is still produced (observational)
        // but the outcome is JudgedWarmup so submit_step_complete won't
        // seal a StepProof for it.
        let judge = RecordingJudge::passing();
        let mut state = fresh_state();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &config_with_threshold(3),
            &judge,
        )
        .await;
        match outcome {
            StepJudgeOutcome::JudgedWarmup {
                threshold,
                baseline,
                verdict,
                ..
            } => {
                assert_eq!(threshold, 3);
                assert_eq!(baseline.successful_count, 1, "this judgement counted");
                assert_eq!(baseline.insufficient_count, 0);
                assert!(verdict.sufficient, "verdict still produced for telemetry");
            }
            other => panic!("expected JudgedWarmup, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enforcement_engages_after_threshold_clears() {
        // Threshold = 2. First two passing runs are warmup; third
        // produces full Judged outcome.
        let judge = RecordingJudge::passing();
        let mut state = fresh_state();
        let configs = config_with_threshold(2);

        // Run 1 — warmup.
        let (_, outcome1) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &configs,
            &judge,
        )
        .await;
        assert!(matches!(outcome1, StepJudgeOutcome::JudgedWarmup { .. }));

        // Run 2 — still warmup (counter is 1, threshold is 2).
        let (_, outcome2) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &configs,
            &judge,
        )
        .await;
        assert!(matches!(outcome2, StepJudgeOutcome::JudgedWarmup { .. }));

        // Run 3 — counter at 2 BEFORE this run, threshold cleared.
        // This is the first enforced verdict.
        let (_, outcome3) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &configs,
            &judge,
        )
        .await;
        match outcome3 {
            StepJudgeOutcome::Judged { skill, step_id, .. } => {
                assert_eq!(skill, "linear");
                assert_eq!(step_id, "1");
            }
            other => panic!("run 3 should be Judged (threshold cleared), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn threshold_zero_enforces_immediately_no_warmup() {
        // baseline_threshold = 0 means "enforce from step 1" — right
        // for high-stakes steps that should never be observed before
        // gating. The default fresh_state has zero successes; a fresh
        // counter with threshold=0 must clear immediately, so the
        // outcome must be Judged not JudgedWarmup.
        let judge = RecordingJudge::passing();
        let mut state = fresh_state();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &config_with_threshold(0),
            &judge,
        )
        .await;
        assert!(
            matches!(outcome, StepJudgeOutcome::Judged { .. }),
            "threshold=0 must enforce immediately, got {outcome:?}",
        );
    }

    #[tokio::test]
    async fn warmup_records_insufficient_judgements_for_telemetry() {
        // During warmup, BOTH passing and failing judgements must be
        // recorded so operators can see warmup-time false-positive
        // rates and decide when to lower the threshold. The chain
        // doesn't seal anything during warmup, but the counter still
        // tracks observations.
        let judge = RecordingJudge::failing();
        let mut state = fresh_state();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &mut state,
            &config_with_threshold(3),
            &judge,
        )
        .await;
        match outcome {
            StepJudgeOutcome::JudgedWarmup {
                baseline, verdict, ..
            } => {
                assert!(!verdict.sufficient);
                assert_eq!(baseline.successful_count, 0, "no passing yet");
                assert_eq!(baseline.insufficient_count, 1, "this failing run counted");
            }
            other => panic!("expected JudgedWarmup with failing verdict, got {other:?}"),
        }
    }

    #[test]
    fn parser_round_trips_known_step_names() {
        // Sanity check shared with step_gate (M1.3) — same parsing
        // semantics, kept in sync.
        let r = StepToolRef::parse("mcp__skills__linear__step_3.L2.3").expect("parses");
        assert_eq!(r.skill, "linear");
        assert_eq!(r.step_id, "3.L2.3");
    }
}
