//! Step Judge Hook
//!
//! PostToolUse hook that runs the AI judge against a completed step's
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
//! # What this hook does today (M1.4 stub scope)
//!
//! 1. Detects PostToolUse for a step tool (same `mcp__skills__<skill>__step_<id>`
//!    naming as `step_gate`).
//! 2. Resolves the step's description from the loaded step config (so the
//!    judge prompt knows what "sufficient" looks like for this specific step).
//! 3. Gathers a step-shaped [`Evidence`] from the tool input/output.
//! 4. Invokes the [`JudgeService`] trait (default impl falls through to the
//!    existing `evaluate` so `MultiModelJudge` and `FallbackJudge` keep
//!    working without changes).
//! 5. Returns the [`JudgeVerdict`] inside a [`StepJudgeOutcome`] struct that
//!    M1.5's `submit_step_complete` will consume.
//!
//! Persistence (writing the StepProof to disk, advancing the chain head)
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
//! # Glass break + sentinel authority
//!
//! Same break semantics as the other gates: an active break short-circuits
//! and emits `Outcome::Skipped` so the chain doesn't get a fake verdict
//! during emergency bypass. The hook never blocks via `HookOutput::deny`
//! — failed verdicts surface as a non-sufficient `JudgeVerdict` that
//! `submit_step_complete` will refuse to seal. PostToolUse blocking is
//! the wrong layer for "this step didn't pass"; the chain itself is the
//! enforcement substrate, not this hook's allow/deny return.

use std::collections::HashMap;

use sentinel_domain::evidence::{Evidence, ToolCallEvidence, ToolResultEvidence};
use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
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
    /// Skill has no step config registered — backwards-compat, no verdict.
    NoStepConfig,
    /// Skill has a step config but the specific step_id wasn't found in any
    /// phase. Possible misconfig or stale tool name.
    UnknownStep {
        skill: String,
        step_id: String,
    },
    /// Glass break was active — judge skipped, no verdict.
    Skipped,
    /// Judge ran, here's the verdict + the resolved coordinates so M1.5 can
    /// build the StepProof without reparsing the tool name.
    Judged {
        skill: String,
        phase_id: String,
        step_id: String,
        step_description: String,
        evidence: Evidence,
        verdict: JudgeVerdict,
        judge_model: JudgeModel,
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

/// Resolve `(phase_id, step_description)` for a given `step_id` within a
/// skill's step config. Returns `None` if the step isn't found in any phase.
fn locate_step<'a>(
    skill_steps: &'a SkillSteps,
    step_id: &str,
) -> Option<(&'a str, &'a str)> {
    for phase in &skill_steps.phases {
        for step in &phase.steps {
            if step.id == step_id {
                return Some((phase.phase_id.as_str(), step.description.as_str()));
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

/// Build a step-shaped Evidence blob from PostToolUse `HookInput`.
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
            timestamp: timestamp.clone(),
        });
    }

    if let Some(tool_result) = input.tool_result.as_ref() {
        // Heuristic for "did the tool succeed": presence of an error field
        // or a top-level boolean false ⇒ failure. Otherwise assume success.
        // Real M1.5 evidence work will replace this with structured exit
        // codes from sentinel's tool-result capture path.
        let success = !tool_result.get("error").is_some()
            && tool_result.as_bool().map_or(true, |b| b);
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
    custom.insert("step_tool_name".into(), serde_json::Value::String(tool_name));
    if let Some(tool_input) = input.tool_input.clone() {
        custom.insert("step_tool_input".into(), tool_input);
    }
    if let Some(tool_result) = input.tool_result.clone() {
        custom.insert("step_tool_result".into(), tool_result);
    }
    evidence.custom = serde_json::Value::Object(custom);

    evidence
}

/// Process a step-judge hook event (PostToolUse).
///
/// Returns `(HookOutput, StepJudgeOutcome)`. The `HookOutput` is always
/// `allow` for this hook — we don't block tool calls in PostToolUse, we
/// just produce verdicts. The `StepJudgeOutcome` carries the verdict (or
/// the reason no verdict was produced) up to the caller; M1.5's wiring
/// in `hook_cmd.rs` will consume it to call `submit_step_complete` when
/// `Outcome::Judged`.
pub async fn process(
    input: &HookInput,
    step_configs: &HashMap<String, SkillSteps>,
    judge: &dyn JudgeService,
    glass_break_active: bool,
) -> (HookOutput, StepJudgeOutcome) {
    let tool_name = match &input.tool_name {
        Some(name) => name.as_str(),
        None => return (HookOutput::allow(), StepJudgeOutcome::NotAStepTool),
    };

    let step_ref = match StepToolRef::parse(tool_name) {
        Some(r) => r,
        None => return (HookOutput::allow(), StepJudgeOutcome::NotAStepTool),
    };

    if glass_break_active {
        return (HookOutput::allow(), StepJudgeOutcome::Skipped);
    }

    let skill_steps = match step_configs.get(&step_ref.skill) {
        Some(s) => s,
        None => return (HookOutput::allow(), StepJudgeOutcome::NoStepConfig),
    };

    let (phase_id, step_description) = match locate_step(skill_steps, &step_ref.step_id) {
        Some(pair) => (pair.0.to_string(), pair.1.to_string()),
        None => {
            return (
                HookOutput::allow(),
                StepJudgeOutcome::UnknownStep {
                    skill: step_ref.skill,
                    step_id: step_ref.step_id,
                },
            );
        }
    };

    let evidence = gather_evidence(input);

    // Judge model selection: until #73 lands the per-step `judge:` field,
    // step-level eval defaults to `Sonnet` (the standard tier). When #73
    // ships, this picks from step config metadata.
    let model = JudgeModel::Sonnet;

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
        Ok(verdict) => (
            HookOutput::allow(),
            StepJudgeOutcome::Judged {
                skill: step_ref.skill,
                phase_id,
                step_id: step_ref.step_id,
                step_description,
                evidence,
                verdict: verdict.sanitized(),
                judge_model: model,
            },
        ),
        Err(e) => (
            HookOutput::allow(),
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
        // default impl that delegates to evaluate, ensuring backwards
        // compat for existing implementers (FallbackJudge, MultiModelJudge).
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

    fn linear_step_config() -> HashMap<String, SkillSteps> {
        let mut m = HashMap::new();
        m.insert(
            "linear".to_string(),
            SkillSteps {
                skill: "linear".into(),
                phases: vec![PhaseSteps {
                    phase_id: "claim".into(),
                    steps: vec![WorkflowStep {
                        id: "1".into(),
                        description: "Open PR with Ref FPCRM-XXX".into(),
                        blocker: false,
                    }],
                }],
            },
        );
        m
    }

    #[tokio::test]
    async fn passes_through_non_step_tools() {
        let judge = RecordingJudge::passing();
        let (out, outcome) =
            process(&step_input("Read"), &linear_step_config(), &judge, false).await;
        assert!(matches!(outcome, StepJudgeOutcome::NotAStepTool));
        assert!(out.hook_specific_output.is_none());
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0, "judge must NOT fire on non-step tools");
    }

    #[tokio::test]
    async fn skipped_during_glass_break() {
        let judge = RecordingJudge::passing();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &linear_step_config(),
            &judge,
            true, // glass_break_active
        )
        .await;
        assert!(matches!(outcome, StepJudgeOutcome::Skipped));
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0, "judge must NOT fire during glass break");
    }

    #[tokio::test]
    async fn no_step_config_for_skill_is_a_quiet_noop() {
        let judge = RecordingJudge::passing();
        let configs = HashMap::new(); // empty — no skill registered
        let (_, outcome) = process(
            &step_input("mcp__skills__deploy__step_1"),
            &configs,
            &judge,
            false,
        )
        .await;
        assert!(matches!(outcome, StepJudgeOutcome::NoStepConfig));
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_step_id_in_known_skill_reports_error_outcome() {
        let judge = RecordingJudge::passing();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_99"),
            &linear_step_config(),
            &judge,
            false,
        )
        .await;
        match outcome {
            StepJudgeOutcome::UnknownStep { skill, step_id } => {
                assert_eq!(skill, "linear");
                assert_eq!(step_id, "99");
            }
            other => panic!("expected UnknownStep, got {other:?}"),
        }
        assert_eq!(judge.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn passing_step_produces_judged_outcome_with_verdict() {
        let judge = RecordingJudge::passing();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &linear_step_config(),
            &judge,
            false,
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
        assert_eq!(judge.calls.load(Ordering::SeqCst), 1, "judge fires exactly once per step");
    }

    #[tokio::test]
    async fn failing_verdict_still_returns_judged_outcome() {
        // Insufficient verdicts do NOT cause an error or skip — the chain
        // is the enforcement layer; submit_step_complete refuses to seal
        // the StepProof when verdict.sufficient == false. step_judge's job
        // is just to produce the verdict.
        let judge = RecordingJudge::failing();
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &linear_step_config(),
            &judge,
            false,
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
        let (_, outcome) = process(
            &step_input("mcp__skills__linear__step_1"),
            &linear_step_config(),
            &judge,
            false,
        )
        .await;
        match outcome {
            StepJudgeOutcome::JudgeError { skill, step_id, error } => {
                assert_eq!(skill, "linear");
                assert_eq!(step_id, "1");
                assert!(error.contains("simulated upstream failure"), "got: {error}");
            }
            other => panic!("expected JudgeError, got {other:?}"),
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
