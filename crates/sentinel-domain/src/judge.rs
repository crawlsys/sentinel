//! AI Judge domain types
//!
//! Defines the request/verdict types for AI-powered evidence verification.
//! Infrastructure layer implements the actual API calls.

use serde::{Deserialize, Serialize};

/// Judge criticality tier — determines which AI provider handles the evaluation.
///
/// Names are historical (from Anthropic model family). Actual routing:
/// - `Haiku` → Cerebras GLM-4.7 (fast, simple phases)
/// - `Sonnet` → `OpenAI` GPT-5.3 Codex (standard phases)
/// - `Opus` → Anthropic Opus 4.6 (critical phases — no fallback to weaker tiers)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JudgeModel {
    /// Standard tier — routes to `OpenAI`, falls back to Anthropic
    Sonnet,
    /// Critical tier — routes to Anthropic only, no fallback
    Opus,
    /// Fast tier — routes to Cerebras, falls back to `OpenAI` or Anthropic
    Haiku,
}

impl std::fmt::Display for JudgeModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sonnet => write!(f, "openai/gpt-5.3"),
            Self::Opus => write!(f, "anthropic/opus-4.6"),
            Self::Haiku => write!(f, "cerebras/glm-4.7"),
        }
    }
}

/// The AI judge's verdict on phase evidence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// Whether the evidence is sufficient to prove the phase completed
    pub sufficient: bool,

    /// Confidence level (0.0 - 1.0)
    pub confidence: f64,

    /// Human-readable reasoning
    pub reasoning: String,

    /// If insufficient, what additional evidence is needed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_evidence: Option<Vec<String>>,
}

impl JudgeVerdict {
    /// Clamp confidence to valid range [0.0, 1.0].
    /// **Attack #127 fix**: Deserialized confidence values may be out of range
    /// (e.g., `-99999.0` or `1e308` from a forged judge response). Clamping
    /// prevents logic bypass via out-of-range confidence values.
    fn clamp_confidence(c: f64) -> f64 {
        c.clamp(0.0, 1.0)
    }

    /// Create a passing verdict
    #[must_use]
    pub fn pass(confidence: f64, reasoning: impl Into<String>) -> Self {
        Self {
            sufficient: true,
            confidence: Self::clamp_confidence(confidence),
            reasoning: reasoning.into(),
            requested_evidence: None,
        }
    }

    /// Create a failing verdict
    #[must_use]
    pub fn fail(confidence: f64, reasoning: impl Into<String>, missing: Vec<String>) -> Self {
        Self {
            sufficient: false,
            confidence: Self::clamp_confidence(confidence),
            reasoning: reasoning.into(),
            requested_evidence: Some(missing),
        }
    }

    /// Sanitize a deserialized verdict — clamps confidence to [0.0, 1.0].
    /// Call this after `serde_json::from_str::<JudgeVerdict>()` to ensure
    /// out-of-range values from AI responses don't corrupt proof chains.
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.confidence = Self::clamp_confidence(self.confidence);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pass_verdict() {
        let v = JudgeVerdict::pass(0.95, "All tests passed");
        assert!(v.sufficient);
        assert!(v.confidence > 0.9);
        assert!(v.requested_evidence.is_none());
    }

    #[test]
    fn test_fail_verdict() {
        let v = JudgeVerdict::fail(
            0.8,
            "No integration tests",
            vec!["integration test for /api/users".to_string()],
        );
        assert!(!v.sufficient);
        assert_eq!(v.requested_evidence.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_judge_model_display() {
        assert_eq!(JudgeModel::Sonnet.to_string(), "openai/gpt-5.3");
        assert_eq!(JudgeModel::Opus.to_string(), "anthropic/opus-4.6");
        assert_eq!(JudgeModel::Haiku.to_string(), "cerebras/glm-4.7");
    }
}
