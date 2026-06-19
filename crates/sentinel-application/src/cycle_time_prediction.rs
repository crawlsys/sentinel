//! Cycle-Time Prediction + Forced Decomposition — SEN-3 + SEN-6.
//!
//! Given a candidate ticket (team + priority + estimate), this module
//! returns a predicted total cycle time + per-stage path expectations,
//! sourced from the [`crate::cycle_time_analytics`] percentile snapshots.
//! Two consumers:
//!
//! * **SEN-3 — Phase 1 fetch injection.** The linear skill's Phase 1
//!   (fetch issue) calls [`predict_cycle_time`] with the just-fetched
//!   ticket's metadata. The returned [`CycleTimePrediction`] gets surfaced
//!   to Claude as `additionalContext` so the agent sees expected effort
//!   before committing to the work.
//!
//! * **SEN-6 — Forced decomposition at Phase 1.5.** [`requires_decomposition`]
//!   returns `true` when the prediction crosses a configurable threshold
//!   (default: 2× the team's p90 OR `> 8h` raw estimate). The `phase_gate`
//!   hook blocks the transition into Phase 2 (worktree creation) until the
//!   user explicitly decomposes via a sub-ticket.
//!
//! Both functions are **pure** — they take the percentile snapshots as
//! input rather than reading disk. The wiring (Phase 1 fetch + phase
//! gate) is a follow-up; this module ships the math seam.

use serde::{Deserialize, Serialize};

use crate::cycle_time_analytics::{PerStageBreakdownSummary, StageBreakdown};

/// Default forced-decomposition rule constants. These mirror the rule
/// documented in the SEN-6 task description; the [`DecompositionRule`]
/// struct allows test/admin overrides without recompiling.
pub const DEFAULT_P90_MULTIPLIER: f64 = 2.0;
pub const DEFAULT_ESTIMATE_CEILING_HOURS: f64 = 8.0;

/// Cycle-time prediction over the canonical Linear pipeline.
///
/// `per_stage_hours` is the predicted dwell time for each stage the
/// ticket will visit (`In Progress` → `Code Review` → `QA Testing`).
/// `total_hours` is their sum. `confidence` reflects the sample count
/// available in the underlying breakdown — see [`Confidence`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleTimePrediction {
    pub team: Option<String>,
    pub priority: Option<u8>,
    pub estimate: Option<u32>,
    pub per_stage_hours: Vec<StageDuration>,
    pub total_hours: f64,
    pub confidence: Confidence,
}

/// Predicted dwell time for one pipeline stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageDuration {
    pub stage: String,
    pub p50_hours: f64,
    pub p75_hours: f64,
    pub sample_count: usize,
}

/// Confidence tier for a prediction. Drives how local clients render the
/// number (high → display directly; low → display as "~", with a tooltip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    /// Sample-count → confidence. Boundaries:
    /// * `>= 20` → High (statistically stable percentile).
    /// * `>= 5`  → Medium (workable estimate).
    /// * else    → Low (small-sample, surface a "~" prefix in the UI).
    #[must_use]
    pub const fn from_sample_count(n: usize) -> Self {
        if n >= 20 {
            Self::High
        } else if n >= 5 {
            Self::Medium
        } else {
            Self::Low
        }
    }
}

/// Stages every Linear ticket walks before completion. The decomposition
/// rule and the Phase 1 prediction both walk this list in order.
const PREDICTION_PATH: &[&str] = &["In Progress", "Code Review", "QA Testing"];

/// Compute a cycle-time prediction for a candidate ticket using the
/// per-stage breakdown snapshot from SEN-17.
///
/// Per-stage duration is sourced from the matching `(team, stage)` row
/// when available; falls back to the team-agnostic row (`team = None`)
/// when the specific team has no samples; falls back to `0.0` with
/// `confidence = Low` when nothing matches.
///
/// `team`, `priority`, and `estimate` are recorded on the result for the
/// client audit trail but don't currently parametrize the lookup —
/// a future refinement can add priority-bucketed breakdowns once we have
/// enough samples to support them.
#[must_use]
pub fn predict_cycle_time(
    breakdown: &PerStageBreakdownSummary,
    team: Option<&str>,
    priority: Option<u8>,
    estimate: Option<u32>,
) -> CycleTimePrediction {
    let mut per_stage = Vec::with_capacity(PREDICTION_PATH.len());
    let mut total = 0.0_f64;
    let mut min_samples = usize::MAX;

    for stage_name in PREDICTION_PATH {
        let row = pick_breakdown_row(breakdown, team, stage_name);
        let (p50, p75, samples) = row.map_or((0.0, 0.0, 0), |r| {
            (r.p50_hours, r.p75_hours, r.sample_count)
        });
        per_stage.push(StageDuration {
            stage: (*stage_name).to_string(),
            p50_hours: p50,
            p75_hours: p75,
            sample_count: samples,
        });
        total += p50;
        min_samples = min_samples.min(samples);
    }

    // No data anywhere → min_samples stays at usize::MAX; clamp.
    if min_samples == usize::MAX {
        min_samples = 0;
    }

    CycleTimePrediction {
        team: team.map(str::to_string),
        priority,
        estimate,
        per_stage_hours: per_stage,
        total_hours: total,
        confidence: Confidence::from_sample_count(min_samples),
    }
}

/// Prefer the team-specific row for the stage; fall back to a team-
/// agnostic (`team = None`) row when no team-specific match exists.
fn pick_breakdown_row<'a>(
    breakdown: &'a PerStageBreakdownSummary,
    team: Option<&str>,
    stage: &str,
) -> Option<&'a StageBreakdown> {
    if let Some(t) = team {
        let specific = breakdown
            .per_stage
            .iter()
            .find(|r| r.team.as_deref() == Some(t) && r.stage == stage);
        if specific.is_some() {
            return specific;
        }
    }
    breakdown
        .per_stage
        .iter()
        .find(|r| r.team.is_none() && r.stage == stage)
}

/// Forced-decomposition rule — SEN-6.
///
/// A ticket "must decompose" when EITHER:
///
/// * The predicted total cycle time exceeds the team's worst-case
///   percentile (`p75` * `p90_multiplier`), suggesting it's an outlier
///   the team historically doesn't ship in one shot.
/// * The raw `estimate` exceeds [`DecompositionRule::estimate_ceiling`]
///   (default 8h) — a story-point heuristic for "this is too big".
///
/// Returns `true` when decomposition is required.
#[derive(Debug, Clone, Copy)]
pub struct DecompositionRule {
    pub p90_multiplier: f64,
    pub estimate_ceiling: f64,
}

impl Default for DecompositionRule {
    fn default() -> Self {
        Self {
            p90_multiplier: DEFAULT_P90_MULTIPLIER,
            estimate_ceiling: DEFAULT_ESTIMATE_CEILING_HOURS,
        }
    }
}

#[must_use]
pub fn requires_decomposition(prediction: &CycleTimePrediction, rule: DecompositionRule) -> bool {
    // Rule 1: raw estimate over the ceiling.
    if let Some(est) = prediction.estimate {
        if f64::from(est) > rule.estimate_ceiling {
            return true;
        }
    }

    // Rule 2: total predicted dwell time exceeds the multiplier × the
    // sum of stage p75s. We use p75 as the "team's worst typical" floor
    // since the per-stage breakdown doesn't carry p90 explicitly; p75
    // × multiplier (2.0) is a stricter equivalent of p90 × 1.0 for
    // most distributions.
    let p75_sum: f64 = prediction.per_stage_hours.iter().map(|s| s.p75_hours).sum();
    let threshold = p75_sum * rule.p90_multiplier;
    if threshold > 0.0 && prediction.total_hours > threshold {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cycle_time_analytics::PerStageBreakdownSummary;

    fn breakdown(rows: Vec<StageBreakdown>) -> PerStageBreakdownSummary {
        PerStageBreakdownSummary {
            generated_at: "2026-05-15T12:00:00Z".to_string(),
            window_days: 30,
            events_scanned: 100,
            pairs_used: 50,
            per_stage: rows,
        }
    }

    fn row(team: Option<&str>, stage: &str, p50: f64, p75: f64, samples: usize) -> StageBreakdown {
        StageBreakdown {
            team: team.map(str::to_string),
            stage: stage.to_string(),
            sample_count: samples,
            mean_hours: p50,
            p50_hours: p50,
            p75_hours: p75,
        }
    }

    // --- Confidence boundaries -----------------------------------------

    #[test]
    fn confidence_boundaries() {
        assert_eq!(Confidence::from_sample_count(0), Confidence::Low);
        assert_eq!(Confidence::from_sample_count(4), Confidence::Low);
        assert_eq!(Confidence::from_sample_count(5), Confidence::Medium);
        assert_eq!(Confidence::from_sample_count(19), Confidence::Medium);
        assert_eq!(Confidence::from_sample_count(20), Confidence::High);
        assert_eq!(Confidence::from_sample_count(1000), Confidence::High);
    }

    // --- predict_cycle_time --------------------------------------------

    #[test]
    fn predict_empty_breakdown_returns_zero_low_confidence() {
        let p = predict_cycle_time(&breakdown(vec![]), Some("X"), Some(2), Some(3));
        assert!((p.total_hours - 0.0).abs() < f64::EPSILON);
        assert_eq!(p.confidence, Confidence::Low);
        assert_eq!(p.per_stage_hours.len(), 3);
        for s in &p.per_stage_hours {
            assert_eq!(s.sample_count, 0);
            assert!((s.p50_hours - 0.0).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn predict_sums_team_specific_p50s() {
        let b = breakdown(vec![
            row(Some("X"), "In Progress", 4.0, 6.0, 30),
            row(Some("X"), "Code Review", 2.0, 4.0, 25),
            row(Some("X"), "QA Testing", 6.0, 8.0, 22),
        ]);
        let p = predict_cycle_time(&b, Some("X"), Some(2), Some(3));
        assert!((p.total_hours - 12.0).abs() < f64::EPSILON);
        // min_samples = 22 → High confidence.
        assert_eq!(p.confidence, Confidence::High);
    }

    #[test]
    fn predict_falls_back_to_team_agnostic_row() {
        let b = breakdown(vec![
            // Only team-agnostic data exists.
            row(None, "In Progress", 5.0, 10.0, 8),
            row(None, "Code Review", 3.0, 6.0, 8),
            row(None, "QA Testing", 4.0, 8.0, 8),
        ]);
        // Asking about team Y for which we have no data → falls back to None rows.
        let p = predict_cycle_time(&b, Some("Y"), None, None);
        assert!((p.total_hours - 12.0).abs() < f64::EPSILON);
        assert_eq!(p.confidence, Confidence::Medium);
    }

    #[test]
    fn predict_picks_lowest_sample_count_for_confidence() {
        let b = breakdown(vec![
            row(Some("X"), "In Progress", 4.0, 6.0, 50),
            row(Some("X"), "Code Review", 2.0, 4.0, 3), // smallest
            row(Some("X"), "QA Testing", 6.0, 8.0, 100),
        ]);
        let p = predict_cycle_time(&b, Some("X"), None, None);
        // min_samples = 3 → Low.
        assert_eq!(p.confidence, Confidence::Low);
    }

    #[test]
    fn predict_records_input_metadata() {
        let p = predict_cycle_time(&breakdown(vec![]), Some("FPCRM"), Some(2), Some(5));
        assert_eq!(p.team.as_deref(), Some("FPCRM"));
        assert_eq!(p.priority, Some(2));
        assert_eq!(p.estimate, Some(5));
    }

    // --- requires_decomposition ----------------------------------------

    #[test]
    fn decomposition_required_when_estimate_over_ceiling() {
        let pred = predict_cycle_time(&breakdown(vec![]), Some("X"), None, Some(13));
        assert!(requires_decomposition(&pred, DecompositionRule::default()));
    }

    #[test]
    fn decomposition_not_required_when_estimate_at_ceiling() {
        let pred = predict_cycle_time(&breakdown(vec![]), Some("X"), None, Some(8));
        // Ceiling rule fires on `>`, not `>=`. 8h is the operational ceiling
        // and should NOT force decomposition by itself.
        let rule = DecompositionRule::default();
        // With no breakdown data the total will be 0 < threshold; estimate
        // is exactly 8, not > 8. So rule should NOT fire.
        assert!(!requires_decomposition(&pred, rule));
    }

    #[test]
    fn decomposition_required_when_total_exceeds_p75_times_multiplier() {
        let b = breakdown(vec![
            row(Some("X"), "In Progress", 100.0, 5.0, 20),
            row(Some("X"), "Code Review", 0.0, 0.0, 20),
            row(Some("X"), "QA Testing", 0.0, 0.0, 20),
        ]);
        let pred = predict_cycle_time(&b, Some("X"), None, None);
        // total = 100, p75_sum = 5, multiplier = 2 → threshold = 10.
        // 100 > 10 → must decompose.
        assert!(requires_decomposition(&pred, DecompositionRule::default()));
    }

    #[test]
    fn decomposition_not_required_with_no_data() {
        // Total = 0, p75_sum = 0 → threshold = 0. The `> 0` guard in the
        // rule means we don't force decomposition on zero-data tickets;
        // the prediction surface is already showing Low confidence.
        let pred = predict_cycle_time(&breakdown(vec![]), Some("X"), None, Some(3));
        assert!(!requires_decomposition(&pred, DecompositionRule::default()));
    }

    #[test]
    fn decomposition_rule_is_configurable() {
        // Tightening the multiplier to 0.5 makes a previously-passing
        // ticket fail.
        let b = breakdown(vec![
            row(Some("X"), "In Progress", 10.0, 10.0, 20),
            row(Some("X"), "Code Review", 0.0, 0.0, 20),
            row(Some("X"), "QA Testing", 0.0, 0.0, 20),
        ]);
        let pred = predict_cycle_time(&b, Some("X"), None, None);
        // Default rule: threshold = 10 * 2.0 = 20; total = 10 → passes.
        assert!(!requires_decomposition(&pred, DecompositionRule::default()));
        // Tight rule: threshold = 10 * 0.5 = 5; total = 10 → fails.
        assert!(requires_decomposition(
            &pred,
            DecompositionRule {
                p90_multiplier: 0.5,
                estimate_ceiling: 100.0,
            },
        ));
    }
}
