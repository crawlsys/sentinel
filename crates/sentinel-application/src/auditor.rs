//! Static (in-memory) implementations of
//! [`AuditorPort`](sentinel_domain::ports::AuditorPort).
//!
//! [`StaticAuditor`] is the application-layer test helper for A3
//! (`docs/a3-dry-run-then-commit.md`). Mirrors the
//! [`StaticReversibilityClassifier`](crate::reversibility_classifier::StaticReversibilityClassifier)
//! pattern: hooks that take a `&dyn AuditorPort` use this in their unit
//! tests, with the production adapter (Phase 3b in
//! `sentinel-infrastructure`) doing real LLM calls via the OpenRouter
//! gateway pattern.

use sentinel_domain::dry_run::{
    AuditorAxes, AuditorDecision, AuditorError, AuditorVerdict, DryRunRequest,
};
use sentinel_domain::ports::AuditorPort;

/// In-memory [`AuditorPort`] returning a pre-configured verdict (or error)
/// regardless of the dry-run contents.
///
/// Useful for hook tests that want to exercise the dispatch tree
/// (Pass/Block/AuditorError variants) without standing up an LLM. The
/// production [`RigAuditor`](sentinel_infrastructure::dry_run_auditor)
/// (Phase 3b) implements the same trait against a real vendor.
#[derive(Debug, Clone)]
pub struct StaticAuditor {
    verdict: Result<AuditorVerdict, AuditorError>,
}

impl StaticAuditor {
    /// Construct from an explicit verdict (or error).
    #[must_use]
    pub fn new(verdict: Result<AuditorVerdict, AuditorError>) -> Self {
        Self { verdict }
    }

    /// Convenience: auditor returns Pass with the given confidence + the
    /// canonical "no concerns" reasoning. Axes default to a uniform 0.9
    /// across all four — adjust via [`Self::with_axes`] when a test
    /// needs specific per-axis scores.
    #[must_use]
    pub fn pass(confidence: f32) -> Self {
        Self::new(Ok(AuditorVerdict {
            decision: AuditorDecision::Pass,
            confidence,
            axes: AuditorAxes::new(0.9, 0.9, 0.9, 0.9),
            reasoning: "looks good".to_string(),
            auditor_model: "test:auditor".to_string(),
        }))
    }

    /// Convenience: auditor returns Block with the given reason. Default
    /// confidence 0.95, low-axis safety (0.2) so the verdict reads
    /// plausibly to consumers of `AuditorAxes::weakest_axis`.
    #[must_use]
    pub fn block(reason: impl Into<String>) -> Self {
        Self::new(Ok(AuditorVerdict {
            decision: AuditorDecision::Block {
                reason: reason.into(),
            },
            confidence: 0.95,
            axes: AuditorAxes::new(0.5, 0.5, 0.2, 0.5),
            reasoning: "concerns".to_string(),
            auditor_model: "test:auditor".to_string(),
        }))
    }

    /// Convenience: auditor returns an error.
    #[must_use]
    pub fn err(err: AuditorError) -> Self {
        Self::new(Err(err))
    }

    /// Override the axes returned by [`Self::pass`] / [`Self::block`].
    /// Useful when a test cares about the specific `weakest_axis()` result.
    #[must_use]
    pub fn with_axes(mut self, axes: AuditorAxes) -> Self {
        if let Ok(ref mut v) = self.verdict {
            v.axes = axes;
        }
        self
    }

    /// Override the `auditor_model` identifier on the returned verdict.
    /// Default `"test:auditor"` is fine for most tests; production-shape
    /// model strings (`"anthropic:claude-opus-4-7"`, `"openai:gpt-5.5"`)
    /// matter for tests that assert on proof-chain attribution.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        if let Ok(ref mut v) = self.verdict {
            v.auditor_model = model.into();
        }
        self
    }
}

impl AuditorPort for StaticAuditor {
    fn score(&self, _dry_run: &DryRunRequest) -> Result<AuditorVerdict, AuditorError> {
        match &self.verdict {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(e.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use sentinel_domain::ReversibilityClass;

    fn fixture_dry_run() -> DryRunRequest {
        DryRunRequest::new(
            "sess-1",
            "Bash",
            serde_json::json!({"command": "git push"}),
            ReversibilityClass::Irreversible,
            Utc::now(),
        )
    }

    #[test]
    fn pass_returns_pass_verdict_with_canned_axes() {
        let auditor = StaticAuditor::pass(0.9);
        let verdict = auditor.score(&fixture_dry_run()).unwrap();
        assert!(verdict.decision.is_pass());
        assert!((verdict.confidence - 0.9).abs() < f32::EPSILON);
        // Default axes: uniform 0.9
        assert!((verdict.axes.mean() - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn block_returns_block_with_reason() {
        let auditor = StaticAuditor::block("policy violation");
        let verdict = auditor.score(&fixture_dry_run()).unwrap();
        match &verdict.decision {
            AuditorDecision::Block { reason } => assert_eq!(reason, "policy violation"),
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn err_returns_error() {
        let auditor = StaticAuditor::err(AuditorError::Unavailable("down".into()));
        let result = auditor.score(&fixture_dry_run());
        assert!(matches!(
            result,
            Err(AuditorError::Unavailable(msg)) if msg == "down"
        ));
    }

    #[test]
    fn with_axes_overrides_defaults() {
        let auditor =
            StaticAuditor::pass(0.9).with_axes(AuditorAxes::new(0.1, 0.2, 0.3, 0.4));
        let verdict = auditor.score(&fixture_dry_run()).unwrap();
        assert!((verdict.axes.correctness - 0.1).abs() < f32::EPSILON);
        let (weakest, _) = verdict.axes.weakest_axis();
        assert_eq!(weakest, "correctness");
    }

    #[test]
    fn with_model_overrides_default_identifier() {
        let auditor = StaticAuditor::pass(0.9).with_model("anthropic:claude-opus-4-7");
        let verdict = auditor.score(&fixture_dry_run()).unwrap();
        assert_eq!(verdict.auditor_model, "anthropic:claude-opus-4-7");
    }

    #[test]
    fn implements_auditor_port_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StaticAuditor>();
    }

    #[test]
    fn usable_through_port_trait_object() {
        let auditor = StaticAuditor::pass(0.95);
        let port: &dyn AuditorPort = &auditor;
        let verdict = port.score(&fixture_dry_run()).unwrap();
        assert!(verdict.decision.is_pass());
    }
}
