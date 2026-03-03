//! AI Judge Service
//!
//! Orchestrates AI evaluation of phase evidence.
//! Uses Sonnet 4.6 for most phases, Opus 4.6 for critical ones.

use anyhow::Result;

use sentinel_domain::evidence::Evidence;
use sentinel_domain::judge::{JudgeModel, JudgeVerdict};

/// Port for AI judge — infrastructure implements with Anthropic API
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

/// Fallback judge that always passes — used when AI is unavailable
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
        Ok(JudgeVerdict::default_pass())
    }
}
