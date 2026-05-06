//! AI Judge Service
//!
//! Orchestrates AI evaluation of phase evidence.
//! Routes to Cerebras (fast), OpenAI (normal), or Anthropic (critical)
//! via the `MultiModelJudge` in infrastructure.

use anyhow::{bail, Result};

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

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
        let synthesized_objective = format!(
            "STEP {step_id} of phase '{phase_id}' (skill '{skill}'): {step_description}"
        );
        // Compose a synthetic phase id so log/trace output distinguishes
        // step-level evaluations from true phase-level ones.
        let synthetic_phase = format!("{phase_id}.{step_id}");
        self.evaluate(skill, &synthetic_phase, &synthesized_objective, evidence, model)
            .await
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
