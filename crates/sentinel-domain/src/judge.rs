//! AI Judge domain types
//!
//! Defines the request/verdict types for AI-powered evidence verification.
//! Infrastructure layer implements the actual API calls.

use serde::{Deserialize, Serialize};

/// Judge criticality tier — determines which model evaluates the evidence.
///
/// All tiers route through `OpenRouter` (single gateway, single auth surface,
/// automatic provider failover). The four tiers are the operator-pinned model
/// set — Codex / Sonnet / Opus / Kimi — one per vendor for cross-distribution
/// diversity. Routing (operator-specified 2026-05-25):
///
/// - `Codex` → `openai/gpt-5.5-pro` (`OpenAI`; the model that powers Codex)
/// - `Kimi` → `moonshotai/kimi-k2.6` (Moonshot; OSS frontier, Eastern
///   training distribution for adversarial diversity)
/// - `Sonnet` → `anthropic/claude-sonnet-4.6` (Anthropic; balanced daily tier)
/// - `Opus` → `anthropic/claude-opus-4.7` (Anthropic; deepest reasoning,
///   highest-stakes work)
///
/// Step configs declare the tier; multi-judge dispatch is a runtime concern
/// that pairs models from different vendors (Codex/openai + Kimi/moonshot +
/// Opus/anthropic) so disagreement signals come from genuinely different
/// blind spots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JudgeModel {
    /// Codex tier — `OpenAI` GPT-5.5-pro via `OpenRouter` (the model that powers
    /// Codex). Operator-pinned per the four-model judge set
    /// (Codex / Sonnet / Opus / Kimi).
    Codex,
    /// Review tier (DEFAULT) — Moonshot Kimi K2-Thinking via `OpenRouter`. The
    /// callable Kimi variant (`kimi-k2.6` 404s on the operator's provider
    /// routing). Eastern training distribution adds diversity vs the Western
    /// closed-model trio for adversarial review.
    Kimi,
    /// Cross-vendor verification — `OpenAI` GPT-5.5 via `OpenRouter` (newest
    /// frontier; #1 reasoning, 2.8s live). Pairs with Kimi at the `critical`
    /// trust tier so disagreement signals are real (different model families,
    /// different blind spots). Was the stale `gpt-5.4`.
    Sonnet,
    /// Critical-strict tier — Anthropic Claude Opus 4.7 via `OpenRouter`. Third
    /// leg of the trio for highest-stakes audit-grade work.
    Opus,
}

impl std::fmt::Display for JudgeModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Codex => write!(f, "openai/gpt-5.5-pro"),
            Self::Kimi => write!(f, "moonshotai/kimi-k2.6"),
            Self::Sonnet => write!(f, "anthropic/claude-sonnet-4.6"),
            Self::Opus => write!(f, "anthropic/claude-opus-4.7"),
        }
    }
}

impl JudgeModel {
    /// Default judge for skill phases that don't specify a tier explicitly.
    /// Lands on Kimi K2-Thinking — Eastern-distribution diversity at a
    /// fraction of the closed-frontier price, and (unlike `kimi-k2.6`)
    /// actually callable on the operator's `OpenRouter` key.
    #[must_use]
    pub const fn default_review_tier() -> Self {
        Self::Kimi
    }

    /// `OpenRouter` model identifier for this tier — what the API expects in
    /// the `model` field of a chat-completion request. Every slug here was
    /// confirmed to resolve + respond by live probe (see the type docs).
    #[must_use]
    pub const fn openrouter_model_id(self) -> &'static str {
        match self {
            Self::Codex => "openai/gpt-5.5-pro",
            Self::Kimi => "moonshotai/kimi-k2.6",
            Self::Sonnet => "anthropic/claude-sonnet-4.6",
            Self::Opus => "anthropic/claude-opus-4.7",
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
        assert_eq!(JudgeModel::Codex.to_string(), "openai/gpt-5.5-pro");
        assert_eq!(JudgeModel::Kimi.to_string(), "moonshotai/kimi-k2.6");
        assert_eq!(JudgeModel::Sonnet.to_string(), "anthropic/claude-sonnet-4.6");
        assert_eq!(JudgeModel::Opus.to_string(), "anthropic/claude-opus-4.7");
    }

    #[test]
    fn test_openrouter_model_id_matches_display() {
        // The OpenRouter model id must be exactly the Display representation
        // — they're sent to the same endpoint as the same string. Drift
        // between the two is a silent footgun.
        for model in [
            JudgeModel::Codex,
            JudgeModel::Kimi,
            JudgeModel::Sonnet,
            JudgeModel::Opus,
        ] {
            assert_eq!(model.to_string(), model.openrouter_model_id());
        }
    }

    #[test]
    fn test_default_review_tier_is_kimi() {
        // Cost + diversity argument: Kimi K2.6 is the default for every
        // step that doesn't explicitly opt into a different tier. If this
        // changes, update CONTRIBUTING.md's pluggable-judges section too.
        assert_eq!(JudgeModel::default_review_tier(), JudgeModel::Kimi);
    }

    #[test]
    fn test_judge_model_serde_roundtrip() {
        // kebab-case rename is load-bearing — TOML configs spell tiers as
        // `kimi`, `sonnet`, etc. and a lossy round-trip would silently
        // demote configs to the default tier on reload.
        for model in [
            JudgeModel::Codex,
            JudgeModel::Kimi,
            JudgeModel::Sonnet,
            JudgeModel::Opus,
        ] {
            let s = serde_json::to_string(&model).unwrap();
            let back: JudgeModel = serde_json::from_str(&s).unwrap();
            assert_eq!(model, back, "round-trip lost data for {s}");
        }
    }

    #[test]
    fn test_judge_model_kebab_serialization() {
        // Pin the wire format. If serde renames change, downstream TOML
        // configs that spell `kimi` as a tier name will silently break.
        assert_eq!(
            serde_json::to_string(&JudgeModel::Kimi).unwrap(),
            "\"kimi\""
        );
        assert_eq!(
            serde_json::to_string(&JudgeModel::Sonnet).unwrap(),
            "\"sonnet\""
        );
    }
}
