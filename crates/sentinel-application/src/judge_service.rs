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
