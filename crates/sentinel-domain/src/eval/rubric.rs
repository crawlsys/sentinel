//! A12 — Eval scoring rubric domain types.
//!
//! Per `docs/a12-external-benchmarks.md` §3.3. Six axes, each scored
//! 0.0-1.0 with operator-weighted aggregation. `outcome_realism` is
//! only valid when `gold_outcomes` is present + the source isn't
//! synthetic (enforced at the `EvalCase` level via
//! [`EvalCase::outcome_scoring_valid`](super::case::EvalCase::outcome_scoring_valid)).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Axes
// ---------------------------------------------------------------------------

/// The 6 BA-Eval scoring axes per spec §3.3.
///
/// First five mirror BA5's adversarial-critique axes (citation
/// density, requirements coverage, alternatives seriousness, tonal
/// calibration, stakeholder fit). `OutcomeRealism` is unique to A12
/// because BA5 critiques pre-shipment artifacts and outcomes aren't
/// known yet at critique time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EvalAxis {
    /// Every claim cited; citations match source; right type of source.
    /// Mirrors BA5 axes 3.1 + 3.3.
    CitationDensityAccuracy,
    /// Every gold-recommendation is addressed or explicitly
    /// trade-off-ed; no recommendations untraceable to stated need.
    /// Mirrors BA5 axis 3.5 + BA3.
    RequirementsCoverage,
    /// Top-2 alternatives steelmanned; not strawmanned.
    /// Mirrors BA5 axis 3.2.
    AlternativesSeriousness,
    /// Confidence proportional to evidence; no spin; explicit
    /// uncertainty where warranted. Mirrors BA5 axis 3.4.
    TonalCalibration,
    /// Agent's recommendation matches or substantively reasons
    /// about what actually happened. Only valid when `gold_outcomes`
    /// is present + source isn't synthetic. Rare to score; very
    /// high signal when available.
    OutcomeRealism,
    /// Output is shaped appropriately for the stated audience (exec
    /// / board / customer / internal team).
    StakeholderFit,
}

impl EvalAxis {
    /// Stable string identifier for serialization + reporting.
    /// `snake_case` to match the spec.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::CitationDensityAccuracy => "citation_density_accuracy",
            Self::RequirementsCoverage => "requirements_coverage",
            Self::AlternativesSeriousness => "alternatives_seriousness",
            Self::TonalCalibration => "tonal_calibration",
            Self::OutcomeRealism => "outcome_realism",
            Self::StakeholderFit => "stakeholder_fit",
        }
    }

    /// Returns `true` iff this axis requires gold-outcome data to
    /// score validly. Currently only `OutcomeRealism`.
    #[must_use]
    pub const fn requires_outcomes(self) -> bool {
        matches!(self, Self::OutcomeRealism)
    }
}

/// One axis's contribution to a scored case. `raw` is the unweighted
/// 0.0-1.0 score; `weight` is the rubric's weight for this axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalAxisScore {
    pub axis: EvalAxis,
    /// Unweighted 0.0-1.0 score. Clamped at construction; values
    /// outside the range are clipped (not rejected) so scorers
    /// can't produce out-of-range artifacts.
    pub raw: f32,
    /// Weight per the case's `ScoringRubric`. Echoed here so an
    /// individual `EvalAxisScore` is self-describing for downstream
    /// reporting.
    pub weight: f32,
}

impl EvalAxisScore {
    /// Construct, clamping `raw` to `[0.0, 1.0]`.
    #[must_use]
    pub const fn new(axis: EvalAxis, raw: f32, weight: f32) -> Self {
        Self {
            axis,
            raw: raw.clamp(0.0, 1.0),
            weight,
        }
    }

    /// Weighted contribution = `raw * weight`.
    #[must_use]
    pub fn weighted(&self) -> f32 {
        self.raw * self.weight
    }
}

// ---------------------------------------------------------------------------
// ScoringRubric
// ---------------------------------------------------------------------------

/// Per-axis weights for a case. Sums of weights aren't constrained
/// (operators may run un-normalized rubrics); reporting divides the
/// weighted-sum by the sum-of-weights to derive a composite 0.0-1.0.
///
/// `ScoringRubric::ba_default()` returns the spec's recommended
/// starting weights. Operators override per-case for archetype-specific
/// emphasis (e.g., pricing-archetype cases weight `TonalCalibration`
/// higher because exec-facing pricing pitches over-promise frequently).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoringRubric {
    pub citation_density_accuracy: f32,
    pub requirements_coverage: f32,
    pub alternatives_seriousness: f32,
    pub tonal_calibration: f32,
    pub outcome_realism: f32,
    pub stakeholder_fit: f32,
}

impl ScoringRubric {
    /// Spec-recommended starting weights. All axes weighted 1.0
    /// except `OutcomeRealism` which is 2.0 (when scorable, it's
    /// the highest-signal axis per spec §3.3).
    #[must_use]
    pub const fn ba_default() -> Self {
        Self {
            citation_density_accuracy: 1.0,
            requirements_coverage: 1.0,
            alternatives_seriousness: 1.0,
            tonal_calibration: 1.0,
            outcome_realism: 2.0,
            stakeholder_fit: 1.0,
        }
    }

    /// Return the weight for the given axis.
    #[must_use]
    pub const fn weight(&self, axis: EvalAxis) -> f32 {
        match axis {
            EvalAxis::CitationDensityAccuracy => self.citation_density_accuracy,
            EvalAxis::RequirementsCoverage => self.requirements_coverage,
            EvalAxis::AlternativesSeriousness => self.alternatives_seriousness,
            EvalAxis::TonalCalibration => self.tonal_calibration,
            EvalAxis::OutcomeRealism => self.outcome_realism,
            EvalAxis::StakeholderFit => self.stakeholder_fit,
        }
    }

    /// Sum of all weights. Used as the denominator for the
    /// composite-score normalization. Always > 0 for valid
    /// rubrics; `ba_default` returns 7.0.
    #[must_use]
    pub const fn total_weight(&self) -> f32 {
        self.citation_density_accuracy
            + self.requirements_coverage
            + self.alternatives_seriousness
            + self.tonal_calibration
            + self.outcome_realism
            + self.stakeholder_fit
    }
}

impl Default for ScoringRubric {
    fn default() -> Self {
        Self::ba_default()
    }
}

// ---------------------------------------------------------------------------
// EvalScore
// ---------------------------------------------------------------------------

/// Full score for one case in one run. Per-axis raw + weight scores
/// plus a derived composite. Lives in `runs/{run_id}.jsonl` (one
/// line per scored case).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScore {
    pub case_id: super::case::EvalCaseId,
    pub run_id: super::case::EvalRunId,
    /// Per-axis breakdown.
    pub axis_scores: Vec<EvalAxisScore>,
    /// Composite = `sum(axis.weighted()) / rubric.total_weight()`.
    /// Pre-computed at construction so downstream reporting doesn't
    /// re-derive.
    pub composite: f32,
}

impl EvalScore {
    /// Construct from axis scores + the rubric used. Computes the
    /// composite as the weighted mean over the axes; axes with
    /// weight 0 contribute nothing.
    #[must_use]
    pub fn new(
        case_id: super::case::EvalCaseId,
        run_id: super::case::EvalRunId,
        axis_scores: Vec<EvalAxisScore>,
        rubric: &ScoringRubric,
    ) -> Self {
        let total_weight = rubric.total_weight();
        let weighted_sum: f32 = axis_scores.iter().map(EvalAxisScore::weighted).sum();
        let composite = if total_weight > 0.0 {
            (weighted_sum / total_weight).clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            case_id,
            run_id,
            axis_scores,
            composite,
        }
    }

    /// Find the score for a specific axis. Returns `None` if the
    /// rubric didn't cover this axis (e.g., `OutcomeRealism` on a
    /// case without gold outcomes).
    #[must_use]
    pub fn for_axis(&self, axis: EvalAxis) -> Option<&EvalAxisScore> {
        self.axis_scores.iter().find(|s| s.axis == axis)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::case::{EvalCaseId, EvalRunId};

    #[test]
    fn axis_key_is_snake_case() {
        for (axis, expected) in [
            (
                EvalAxis::CitationDensityAccuracy,
                "citation_density_accuracy",
            ),
            (EvalAxis::RequirementsCoverage, "requirements_coverage"),
            (
                EvalAxis::AlternativesSeriousness,
                "alternatives_seriousness",
            ),
            (EvalAxis::TonalCalibration, "tonal_calibration"),
            (EvalAxis::OutcomeRealism, "outcome_realism"),
            (EvalAxis::StakeholderFit, "stakeholder_fit"),
        ] {
            assert_eq!(axis.key(), expected);
        }
    }

    #[test]
    fn only_outcome_realism_requires_outcomes() {
        assert!(EvalAxis::OutcomeRealism.requires_outcomes());
        for other in [
            EvalAxis::CitationDensityAccuracy,
            EvalAxis::RequirementsCoverage,
            EvalAxis::AlternativesSeriousness,
            EvalAxis::TonalCalibration,
            EvalAxis::StakeholderFit,
        ] {
            assert!(
                !other.requires_outcomes(),
                "{other:?} should not require outcomes"
            );
        }
    }

    #[test]
    fn axis_score_clamps_out_of_range() {
        assert!(
            (EvalAxisScore::new(EvalAxis::TonalCalibration, 1.7, 1.0).raw - 1.0).abs()
                < f32::EPSILON
        );
        assert!(
            (EvalAxisScore::new(EvalAxis::TonalCalibration, -0.3, 1.0).raw - 0.0).abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn weighted_multiplies_raw_by_weight() {
        let s = EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, 0.8, 2.0);
        assert!((s.weighted() - 1.6).abs() < 1e-5);
    }

    #[test]
    fn ba_default_rubric_has_outcome_realism_weighted_2() {
        let r = ScoringRubric::ba_default();
        assert!((r.outcome_realism - 2.0).abs() < f32::EPSILON);
        // 5 axes * 1.0 + 1 axis * 2.0 = 7.0
        assert!((r.total_weight() - 7.0).abs() < f32::EPSILON);
    }

    #[test]
    fn rubric_weight_dispatches_per_axis() {
        let r = ScoringRubric::ba_default();
        assert!((r.weight(EvalAxis::OutcomeRealism) - 2.0).abs() < f32::EPSILON);
        assert!((r.weight(EvalAxis::CitationDensityAccuracy) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn eval_score_composite_is_weighted_mean() {
        let rubric = ScoringRubric::ba_default();
        let axis_scores = vec![
            EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, 0.9, 1.0),
            EvalAxisScore::new(EvalAxis::RequirementsCoverage, 0.8, 1.0),
            EvalAxisScore::new(EvalAxis::AlternativesSeriousness, 0.7, 1.0),
            EvalAxisScore::new(EvalAxis::TonalCalibration, 0.85, 1.0),
            EvalAxisScore::new(EvalAxis::OutcomeRealism, 0.6, 2.0),
            EvalAxisScore::new(EvalAxis::StakeholderFit, 0.9, 1.0),
        ];
        // weighted sum = 0.9 + 0.8 + 0.7 + 0.85 + 1.2 + 0.9 = 5.35
        // total weight = 7.0
        // composite = 5.35 / 7.0 ≈ 0.7642857
        let score = EvalScore::new(
            EvalCaseId::new("c1").unwrap(),
            EvalRunId::new("r1").unwrap(),
            axis_scores,
            &rubric,
        );
        assert!(
            (score.composite - 0.764_285_7).abs() < 1e-3,
            "composite should be weighted-mean; got {}",
            score.composite
        );
    }

    #[test]
    fn for_axis_returns_none_for_unscored_axis() {
        let rubric = ScoringRubric::ba_default();
        let score = EvalScore::new(
            EvalCaseId::new("c1").unwrap(),
            EvalRunId::new("r1").unwrap(),
            vec![EvalAxisScore::new(
                EvalAxis::CitationDensityAccuracy,
                0.9,
                1.0,
            )],
            &rubric,
        );
        assert!(score.for_axis(EvalAxis::OutcomeRealism).is_none());
        assert!(score.for_axis(EvalAxis::CitationDensityAccuracy).is_some());
    }

    #[test]
    fn rubric_roundtrips_through_json() {
        let original = ScoringRubric::ba_default();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ScoringRubric = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvalAxis>();
        assert_send_sync::<EvalAxisScore>();
        assert_send_sync::<ScoringRubric>();
        assert_send_sync::<EvalScore>();
    }
}
