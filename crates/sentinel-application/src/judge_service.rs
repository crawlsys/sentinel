//! AI Judge Service
//!
//! Orchestrates AI evaluation of phase evidence.
//! Routes to Cerebras (fast), OpenAI (normal), or Anthropic (critical)
//! via the `MultiModelJudge` in infrastructure.

use anyhow::{bail, Result};

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};
use sentinel_domain::multi_judge::{JudgeRun, JudgeTrustTier, MultiJudgeVerdict};

/// Port for AI judge — infrastructure implements with Rig LLM providers
#[async_trait::async_trait]
pub trait JudgeService: Send + Sync {
    /// Evaluate evidence for a phase using the specified AI model
    async fn evaluate(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict>;

    /// Evaluate evidence for a single step within a phase. Step granularity
    /// is finer than `evaluate`'s phase granularity — each step's evidence
    /// produces its own [`JudgeVerdict`], which becomes the seed for a
    /// [`StepProof`](sentinel_domain::step_proof::StepProof) appended to the
    /// active proof chain.
    ///
    /// **Default impl** synthesizes a step-shaped objective string and
    /// delegates to `evaluate()`. This keeps existing implementers
    /// (`FallbackJudge`, `MultiModelJudge`) compiling without changes —
    /// they get step-level evaluation for free, just routed through the
    /// phase-level prompting they already do. Implementers that want
    /// step-specific prompting (richer evidence framing, per-step rubrics,
    /// disagreement-aware multi-judge) should override this method.
    ///
    /// `step_description` is the human-readable step intent from the step
    /// config (e.g. "Open PR with Ref FPCRM-XXX"). It maps to the
    /// `phase_objectives` slot when delegating so the judge prompt knows
    /// what "sufficient" means for this step.
    async fn evaluate_step(
        &self,
        skill: &str,
        phase_id: &str,
        step_id: &str,
        step_description: &str,
        evidence: &Evidence,
        model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        // Synthesize a phase-shaped objective string that includes the
        // step coordinates, then delegate. The judge sees enough context
        // to evaluate either way; the default impl is a thin shim, not a
        // shortcut around real verdicts.
        let synthesized_objective =
            format!("STEP {step_id} of phase '{phase_id}' (skill '{skill}'): {step_description}");
        // Compose a synthetic phase id so log/trace output distinguishes
        // step-level evaluations from true phase-level ones.
        let synthetic_phase = format!("{phase_id}.{step_id}");
        self.evaluate(
            skill,
            &synthetic_phase,
            &synthesized_objective,
            evidence,
            model,
        )
        .await
    }

    /// Evaluate evidence with EVERY judge model in the given tier and
    /// synthesize a [`MultiJudgeVerdict`] showing per-judge runs +
    /// disagreement flag (sentinel #69 — pluggable judge backends,
    /// cross-vendor verification).
    ///
    /// This is the OSS-first cross-vendor verification path. The
    /// default tier ([`JudgeTrustTier::Review`]) runs Kimi K2.6 alone;
    /// [`JudgeTrustTier::Critical`] runs Kimi + Sonnet so an attacker
    /// can't game one model's biases; [`JudgeTrustTier::CriticalStrict`]
    /// runs all three (Kimi + Sonnet + Opus) for production-blocking
    /// decisions where the cost of a missed defect dwarfs the cost of
    /// the extra inference calls.
    ///
    /// **Default impl** sequentially calls [`evaluate`](Self::evaluate)
    /// for each model in the tier. Implementers that want parallel
    /// fan-out (faster wall-clock, same cost) should override this
    /// method to spawn concurrent calls. The default is sequential
    /// because the most common case is the single-judge Review tier
    /// where parallelism buys nothing.
    ///
    /// Per-judge errors are NOT propagated as `Err` — a single failed
    /// judge call would mask the verdicts of the others, defeating the
    /// cross-vendor pattern. Failed calls produce a `JudgeRun` with a
    /// `verdict.sufficient=false` carrying the error in `reasoning`,
    /// folded into the verdict's `individuals` list. Callers that
    /// want strict "all judges must respond" semantics check
    /// `individuals.iter().all(|r| !r.verdict.reasoning.starts_with("ERROR:"))`.
    async fn evaluate_multi(
        &self,
        skill: &str,
        phase_id: &str,
        phase_objectives: &str,
        evidence: &Evidence,
        tier: JudgeTrustTier,
    ) -> Result<MultiJudgeVerdict> {
        let mut runs: Vec<JudgeRun> = Vec::new();
        for model in tier.judge_models() {
            let verdict = match self
                .evaluate(skill, phase_id, phase_objectives, evidence, model)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    // Surface the failure as a NOT-sufficient verdict
                    // tagged with ERROR: prefix in reasoning so strict
                    // callers can detect partial failures.
                    JudgeVerdict::fail(0.0, format!("ERROR: judge call failed: {e}"), vec![])
                }
            };
            runs.push(JudgeRun {
                model,
                verdict,
                cost_usd: None,
                provider: None,
            });
        }
        Ok(MultiJudgeVerdict::synthesize(tier, runs))
    }
}

/// Blocking fallback — hard-fails when no AI providers are configured.
/// This ensures phases are NEVER auto-passed without real AI evaluation.
pub struct FallbackJudge;

#[async_trait::async_trait]
impl JudgeService for FallbackJudge {
    async fn evaluate(
        &self,
        _skill: &str,
        _phase_id: &str,
        _phase_objectives: &str,
        _evidence: &Evidence,
        _model: JudgeModel,
    ) -> Result<JudgeVerdict> {
        bail!(
            "AI judge unavailable — no API keys configured. \
             Phase cannot be verified. Set ANTHROPIC_API_KEY, \
             OPENAI_API_KEY, or CEREBRAS_API_KEY."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Test judge that records every model it was called with and
    /// returns a controllable verdict. Lets multi-judge tests assert
    /// every model in a tier got dispatched and disagreement
    /// synthesis works.
    struct RecordingJudge {
        calls: Arc<std::sync::Mutex<Vec<JudgeModel>>>,
        /// Maps model -> (sufficient, confidence). Models not in the
        /// map get sufficient=true, confidence=0.9.
        /// Per-model overrides as a Vec since JudgeModel doesn't impl
        /// Hash. Linear lookup is fine for test-scale model counts.
        verdicts: Vec<(JudgeModel, (bool, f64))>,
        /// When set, this many calls into the run fail with an error.
        fail_after: Option<AtomicUsize>,
    }

    impl RecordingJudge {
        fn new(verdicts: Vec<(JudgeModel, (bool, f64))>) -> Self {
            Self {
                calls: Arc::new(std::sync::Mutex::new(Vec::new())),
                verdicts,
                fail_after: None,
            }
        }

        fn lookup(&self, model: JudgeModel) -> (bool, f64) {
            self.verdicts
                .iter()
                .find(|(m, _)| *m == model)
                .map(|(_, v)| *v)
                .unwrap_or((true, 0.9))
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
            model: JudgeModel,
        ) -> Result<JudgeVerdict> {
            self.calls.lock().unwrap().push(model);
            if let Some(counter) = &self.fail_after {
                let n = counter.fetch_sub(1, Ordering::SeqCst);
                if n == 0 {
                    bail!("simulated transient error");
                }
            }
            let (sufficient, confidence) = self.lookup(model);
            Ok(if sufficient {
                JudgeVerdict::pass(confidence, "ok")
            } else {
                JudgeVerdict::fail(confidence, "no", vec![])
            })
        }
    }

    #[tokio::test]
    async fn evaluate_multi_review_tier_calls_kimi_only() {
        // Default Review tier in this codebase is Kimi K2.6 alone (OSS-first).
        let judge = RecordingJudge::new(Vec::new());
        let calls = judge.calls.clone();
        let verdict = judge
            .evaluate_multi(
                "linear",
                "review",
                "open PR",
                &Evidence::default(),
                JudgeTrustTier::Review,
            )
            .await
            .unwrap();

        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            JudgeTrustTier::Review.judge_models(),
            "Review tier must dispatch to exactly its configured models"
        );
        assert!(
            verdict.sufficient,
            "default verdicts are pass → synthesis is sufficient"
        );
        assert!(!verdict.disagreement);
        assert_eq!(verdict.individuals.len(), calls.len());
    }

    #[tokio::test]
    async fn evaluate_multi_critical_tier_calls_all_models_in_order() {
        // Critical (or CriticalStrict — whichever the codebase defines)
        // pairs Kimi+Sonnet/Opus for cross-vendor verification.
        let tier = JudgeTrustTier::Critical;
        let expected = tier.judge_models();
        assert!(
            expected.len() > 1,
            "Critical tier must include >1 model — that's the point of #69"
        );

        let judge = RecordingJudge::new(Vec::new());
        let calls = judge.calls.clone();
        let _v = judge
            .evaluate_multi(
                "linear",
                "qa-handoff",
                "ship to prod",
                &Evidence::default(),
                tier,
            )
            .await
            .unwrap();
        assert_eq!(*calls.lock().unwrap(), expected);
    }

    #[tokio::test]
    async fn evaluate_multi_synthesizes_disagreement_when_judges_disagree() {
        // Force the FIRST model in the tier to fail (returns sufficient=false)
        // while the rest return sufficient=true. The synthesized verdict
        // must surface disagreement=true.
        let tier = JudgeTrustTier::Critical;
        let models = tier.judge_models();
        let mut verdicts: Vec<(JudgeModel, (bool, f64))> = Vec::new();
        verdicts.push((models[0], (false, 0.6))); // disagrees
        for m in &models[1..] {
            verdicts.push((*m, (true, 0.9)));
        }

        let judge = RecordingJudge::new(verdicts);
        let verdict = judge
            .evaluate_multi(
                "linear",
                "qa-handoff",
                "ship to prod",
                &Evidence::default(),
                tier,
            )
            .await
            .unwrap();

        // Conservative aggregation: any FAIL among individuals →
        // sufficient=false at the multi level. AND disagreement=true.
        assert!(
            !verdict.sufficient,
            "any judge failing must make the multi-verdict fail"
        );
        assert!(
            verdict.disagreement,
            "split votes must surface disagreement=true"
        );
        assert_eq!(verdict.individuals.len(), models.len());
    }

    #[tokio::test]
    async fn evaluate_multi_does_not_abort_on_per_judge_error() {
        // Per-judge transient errors must NOT propagate as Err — they
        // become a sufficient=false JudgeRun with ERROR: in reasoning.
        // Otherwise a single flaky judge can mask the others' verdicts,
        // defeating the cross-vendor pattern's whole purpose.
        let tier = JudgeTrustTier::Critical;
        let mut judge = RecordingJudge::new(Vec::new());
        // Fail on the SECOND call (fail_after starts at 1, decrements to 0
        // on first call which succeeds, then 0→panic on second).
        // Actually we want the first call to succeed and the second to
        // fail. fetch_sub returns the value BEFORE the decrement, so
        // fail_after=1 → first call sees n=1 (no fail), counter becomes 0
        //                 second call sees n=0 (FAIL).
        judge.fail_after = Some(AtomicUsize::new(1));

        let verdict = judge
            .evaluate_multi("linear", "qa-handoff", "ship", &Evidence::default(), tier)
            .await
            .expect("per-judge error must not propagate as Err");

        assert_eq!(verdict.individuals.len(), tier.judge_models().len());
        let has_error_run = verdict
            .individuals
            .iter()
            .any(|r| r.verdict.reasoning.starts_with("ERROR:"));
        assert!(
            has_error_run,
            "the failed judge call must show up as an ERROR-prefixed JudgeRun"
        );
        // And the verdict overall must reflect that one judge failed:
        // sufficient=false because the failing run is fail.
        assert!(!verdict.sufficient);
    }

    #[tokio::test]
    async fn fallback_judge_still_errors_on_single_evaluate() {
        // Back-compat: the original FallbackJudge contract is unchanged.
        let judge = FallbackJudge;
        let err = judge
            .evaluate("x", "y", "z", &Evidence::default(), JudgeModel::Kimi)
            .await
            .expect_err("FallbackJudge must keep erroring");
        assert!(err.to_string().contains("AI judge unavailable"));
    }
}
