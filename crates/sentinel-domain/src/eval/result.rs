//! A12 — Eval run result domain types.
//!
//! Per `docs/a12-external-benchmarks.md` §3.5. These are the runtime
//! envelopes wrapped around [`super::EvalScore`]: one
//! [`EvalCaseResult`] per dispatched case (carrying the candidate
//! output text, timing, and error context alongside the score), and
//! one [`EvalRunResult`] aggregating every case in a benchmark run
//! with summary statistics for dashboard rendering.
//!
//! Pure domain — no ports, no IO. The benchmark runner
//! (A12 Phase 3c) constructs these; the JSONL run store (Phase 3d)
//! persists them; the `sentinel eval run` CLI (Phase 3e) reports on
//! them. This module is the type spine all four phases share.
//!
//! # R5 quarantine boundary
//!
//! Per `docs/policy-replay-mining-quarantine.md`: run results are
//! dispatch input + dashboard signal, never training signal.
//! `candidate_output` may contain redacted-source content per the
//! case's [`super::RedactionLevel`]; downstream consumers must
//! honor the case's [`super::CaseProvenance::is_private_test`] flag
//! before rendering output anywhere public.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{EvalAxis, EvalCaseId, EvalRunId, EvalScore};

// ---------------------------------------------------------------------------
// EvalCaseResult
// ---------------------------------------------------------------------------

/// Outcome of dispatching + scoring a single [`super::EvalCase`].
///
/// Carries the runtime context that [`EvalScore`] doesn't: the
/// candidate output text the agent actually produced, wall-clock
/// timing, and a slot for dispatch / scoring errors so failed cases
/// stay in the run record (rather than disappearing) for telemetry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCaseResult {
    pub case_id: EvalCaseId,
    pub run_id: EvalRunId,
    /// The agent's output text, untrimmed. Empty when `error` is set
    /// and dispatch never produced output.
    pub candidate_output: String,
    /// Scored result. `None` when the case errored before scoring
    /// (dispatch failure, scorer failure, etc.).
    pub score: Option<EvalScore>,
    /// Total wall-clock time from dispatch start to scoring end.
    pub timing_ms: u64,
    pub completed_at: DateTime<Utc>,
    /// When `Some`, dispatch or scoring failed; `score` is guaranteed
    /// `None`. The string carries the error message verbatim from the
    /// failing port for operator triage.
    pub error: Option<String>,
}

impl EvalCaseResult {
    /// True when the case scored without error.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.error.is_none() && self.score.is_some()
    }

    /// True when the case failed dispatch or scoring.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// Convenience accessor for the composite score, if scored.
    #[must_use]
    pub fn composite(&self) -> Option<f32> {
        self.score.as_ref().map(|s| s.composite)
    }
}

// ---------------------------------------------------------------------------
// EvalRunResult
// ---------------------------------------------------------------------------

/// Aggregate outcome of executing the benchmark runner over a corpus
/// (or a subset thereof) as a single named run.
///
/// Lives in `runs/{run_id}.json` per spec §3.5 (the JSONL `runs/
/// {run_id}.jsonl` of per-`EvalScore` lines from the existing
/// rubric module is complementary — it's the score-only view; this
/// is the full-context envelope).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunResult {
    pub run_id: EvalRunId,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub case_results: Vec<EvalCaseResult>,
}

impl EvalRunResult {
    /// Count of cases that scored without error.
    #[must_use]
    pub fn successful_case_count(&self) -> usize {
        self.case_results.iter().filter(|c| c.is_success()).count()
    }

    /// Count of cases that errored during dispatch or scoring.
    #[must_use]
    pub fn errored_case_count(&self) -> usize {
        self.case_results.iter().filter(|c| c.is_error()).count()
    }

    /// Mean composite across successful cases. Returns `None` when
    /// no successful cases (avoids the 0/0 ambiguity — "no data" is
    /// not "score zero").
    #[must_use]
    pub fn mean_composite(&self) -> Option<f32> {
        let composites: Vec<f32> = self
            .case_results
            .iter()
            .filter_map(EvalCaseResult::composite)
            .collect();
        if composites.is_empty() {
            None
        } else {
            #[allow(clippy::cast_precision_loss)]
            let n = composites.len() as f32;
            Some(composites.iter().sum::<f32>() / n)
        }
    }

    /// Mean raw axis score across successful cases. Axes absent from
    /// every case (e.g., `OutcomeRealism` on a corpus without
    /// gold-outcome cases) are omitted from the result.
    #[must_use]
    pub fn mean_per_axis(&self) -> Vec<(EvalAxis, f32)> {
        use std::collections::BTreeMap;
        let mut sums: BTreeMap<EvalAxis, (f32, u32)> = BTreeMap::new();
        for case in &self.case_results {
            let Some(score) = &case.score else { continue };
            for axis_score in &score.axis_scores {
                let entry = sums.entry(axis_score.axis).or_insert((0.0, 0));
                entry.0 += axis_score.raw;
                entry.1 += 1;
            }
        }
        sums.into_iter()
            .map(|(axis, (sum, count))| {
                #[allow(clippy::cast_precision_loss)]
                let mean = sum / count as f32;
                (axis, mean)
            })
            .collect()
    }

    /// Fraction of successful cases whose composite meets or exceeds
    /// `threshold`. Range `[0.0, 1.0]`. Returns `0.0` when there are
    /// no successful cases (operator-facing dashboards prefer a
    /// concrete pass-rate over `None` here — empty corpus → 0% pass).
    #[must_use]
    pub fn pass_rate(&self, threshold: f32) -> f32 {
        let successful: Vec<f32> = self
            .case_results
            .iter()
            .filter_map(EvalCaseResult::composite)
            .collect();
        if successful.is_empty() {
            return 0.0;
        }
        let passing = successful.iter().filter(|c| **c >= threshold).count();
        #[allow(clippy::cast_precision_loss)]
        let n = successful.len() as f32;
        #[allow(clippy::cast_precision_loss)]
        let p = passing as f32;
        p / n
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::{EvalAxisScore, EvalCaseId, EvalRunId, ScoringRubric};
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn successful_case(case_id: &str, run_id: &str, composite_target: f32) -> EvalCaseResult {
        let rubric = ScoringRubric::ba_default();
        // All axes set to `composite_target` so the composite ≈ target.
        let axis_scores = vec![
            EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, composite_target, 1.0),
            EvalAxisScore::new(EvalAxis::RequirementsCoverage, composite_target, 1.0),
            EvalAxisScore::new(EvalAxis::AlternativesSeriousness, composite_target, 1.0),
            EvalAxisScore::new(EvalAxis::TonalCalibration, composite_target, 1.0),
            EvalAxisScore::new(EvalAxis::OutcomeRealism, composite_target, 2.0),
            EvalAxisScore::new(EvalAxis::StakeholderFit, composite_target, 1.0),
        ];
        let score = EvalScore::new(
            EvalCaseId::new(case_id).unwrap(),
            EvalRunId::new(run_id).unwrap(),
            axis_scores,
            &rubric,
        );
        EvalCaseResult {
            case_id: EvalCaseId::new(case_id).unwrap(),
            run_id: EvalRunId::new(run_id).unwrap(),
            candidate_output: "candidate text".to_string(),
            score: Some(score),
            timing_ms: 1234,
            completed_at: ts(1_700_000_000),
            error: None,
        }
    }

    fn errored_case(case_id: &str, run_id: &str) -> EvalCaseResult {
        EvalCaseResult {
            case_id: EvalCaseId::new(case_id).unwrap(),
            run_id: EvalRunId::new(run_id).unwrap(),
            candidate_output: String::new(),
            score: None,
            timing_ms: 50,
            completed_at: ts(1_700_000_100),
            error: Some("dispatch timeout".to_string()),
        }
    }

    #[test]
    fn case_result_is_success_only_when_score_and_no_error() {
        let ok = successful_case("c1", "r1", 0.8);
        assert!(ok.is_success());
        assert!(!ok.is_error());

        let err = errored_case("c2", "r1");
        assert!(!err.is_success());
        assert!(err.is_error());
    }

    #[test]
    fn case_result_composite_pulls_from_score() {
        let ok = successful_case("c1", "r1", 0.5);
        // composite = 0.5 across all axes = 0.5
        let c = ok.composite().expect("composite present");
        assert!((c - 0.5).abs() < 1e-3);

        let err = errored_case("c2", "r1");
        assert!(err.composite().is_none());
    }

    #[test]
    fn run_result_counts_success_and_error() {
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![
                successful_case("c1", "r1", 0.8),
                successful_case("c2", "r1", 0.6),
                errored_case("c3", "r1"),
            ],
        };
        assert_eq!(run.successful_case_count(), 2);
        assert_eq!(run.errored_case_count(), 1);
    }

    #[test]
    fn run_result_mean_composite_averages_successful_only() {
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![
                successful_case("c1", "r1", 0.8),
                successful_case("c2", "r1", 0.6),
                errored_case("c3", "r1"),
            ],
        };
        // Mean of (0.8, 0.6) = 0.7. Errored case excluded.
        let mean = run.mean_composite().expect("at least one successful case");
        assert!((mean - 0.7).abs() < 1e-3, "got {mean}");
    }

    #[test]
    fn run_result_mean_composite_is_none_on_empty_or_all_errored() {
        let empty = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![],
        };
        assert!(empty.mean_composite().is_none());

        let all_errored = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![errored_case("c1", "r1"), errored_case("c2", "r1")],
        };
        assert!(all_errored.mean_composite().is_none());
    }

    #[test]
    fn run_result_mean_per_axis_averages_raw_scores() {
        // Two successful cases: one at 0.4 across-the-board, one at 0.8 across-the-board.
        // Mean per axis = 0.6 for every axis.
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![
                successful_case("c1", "r1", 0.4),
                successful_case("c2", "r1", 0.8),
                errored_case("c3", "r1"),
            ],
        };
        let by_axis = run.mean_per_axis();
        assert_eq!(by_axis.len(), 6, "all 6 axes present");
        for (axis, mean) in &by_axis {
            assert!(
                (mean - 0.6).abs() < 1e-3,
                "axis {axis:?} mean should be 0.6; got {mean}",
            );
        }
    }

    #[test]
    fn run_result_mean_per_axis_empty_when_no_successful_cases() {
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![errored_case("c1", "r1")],
        };
        assert!(run.mean_per_axis().is_empty());
    }

    #[test]
    fn run_result_pass_rate_matches_threshold() {
        // 4 successful + 1 errored. Composites: 0.9, 0.8, 0.5, 0.3.
        // Threshold 0.7 → 2/4 pass = 0.5.
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![
                successful_case("c1", "r1", 0.9),
                successful_case("c2", "r1", 0.8),
                successful_case("c3", "r1", 0.5),
                successful_case("c4", "r1", 0.3),
                errored_case("c5", "r1"),
            ],
        };
        let rate = run.pass_rate(0.7);
        assert!((rate - 0.5).abs() < 1e-3, "got {rate}");
    }

    #[test]
    fn run_result_pass_rate_zero_when_no_successful_cases() {
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![errored_case("c1", "r1")],
        };
        assert!((run.pass_rate(0.7) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn run_result_pass_rate_all_pass_returns_one() {
        let run = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![
                successful_case("c1", "r1", 0.95),
                successful_case("c2", "r1", 0.9),
            ],
        };
        assert!((run.pass_rate(0.8) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn case_result_roundtrips_through_json() {
        let original = successful_case("c1", "r1", 0.7);
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EvalCaseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn case_result_errored_roundtrips_through_json() {
        let original = errored_case("c1", "r1");
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EvalCaseResult = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn run_result_roundtrips_through_json() {
        let original = EvalRunResult {
            run_id: EvalRunId::new("r1").unwrap(),
            started_at: ts(1_700_000_000),
            completed_at: ts(1_700_001_000),
            case_results: vec![
                successful_case("c1", "r1", 0.8),
                errored_case("c2", "r1"),
            ],
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EvalRunResult = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvalCaseResult>();
        assert_send_sync::<EvalRunResult>();
    }
}
