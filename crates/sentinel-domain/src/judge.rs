//! AI Judge domain types
//!
//! Defines the request/verdict types for AI-powered evidence verification.
//! Infrastructure layer implements the actual API calls.

use serde::{Deserialize, Serialize};

use crate::evidence::Evidence;

/// Which AI model to use for judging
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JudgeModel {
    /// Sonnet 4.6 — fast judge (~200-500ms), used for most phases
    Sonnet,
    /// Opus 4.6 — deep judge (~1-3s), used for critical phases (review, qa-handoff)
    Opus,
    /// Haiku 4.5 — lightweight check (~100ms), used for simple validation
    Haiku,
}

impl std::fmt::Display for JudgeModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sonnet => write!(f, "sonnet-4.6"),
            Self::Opus => write!(f, "opus-4.6"),
            Self::Haiku => write!(f, "haiku-4.5"),
        }
    }
}

/// Request to an AI judge to evaluate phase evidence
#[derive(Debug, Clone, Serialize)]
pub struct JudgeRequest {
    /// The phase being judged
    pub phase_id: String,

    /// The skill this phase belongs to
    pub skill: String,

    /// What the phase is supposed to accomplish (from workflow config)
    pub phase_objectives: String,

    /// The collected evidence
    pub evidence: Evidence,

    /// Which model to use
    pub model: JudgeModel,
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
    /// Create a passing verdict
    #[must_use]
    pub fn pass(confidence: f64, reasoning: impl Into<String>) -> Self {
        Self {
            sufficient: true,
            confidence,
            reasoning: reasoning.into(),
            requested_evidence: None,
        }
    }

    /// Create a failing verdict
    #[must_use]
    pub fn fail(confidence: f64, reasoning: impl Into<String>, missing: Vec<String>) -> Self {
        Self {
            sufficient: false,
            confidence,
            reasoning: reasoning.into(),
            requested_evidence: Some(missing),
        }
    }

    /// Create a default pass verdict (used when AI judge is unavailable)
    #[must_use]
    pub fn default_pass() -> Self {
        Self::pass(0.5, "AI judge unavailable — default pass (graceful degradation)")
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
        assert_eq!(JudgeModel::Sonnet.to_string(), "sonnet-4.6");
        assert_eq!(JudgeModel::Opus.to_string(), "opus-4.6");
        assert_eq!(JudgeModel::Haiku.to_string(), "haiku-4.5");
    }
}
