//! A12 Phase 3c — benchmark runner use case.
//!
//! Given a set of [`EvalCase`]s, a candidate-output producer, and a
//! [`EvalScorerPort`], produce an [`EvalRunResult`] that aggregates
//! every case (successful + errored) with per-case timing and a
//! composite score.
//!
//! ## Why a closure for the producer, not a port?
//!
//! The "candidate output" comes from whatever's being benchmarked —
//! a live LLM call via the A2 router, an MCP-orchestrated BA
//! pipeline, a static fixture file, etc. Each caller picks the
//! strategy that fits its context, so the runner takes a closure
//! and the call site provides the producer. The CLI (Phase 3e) will
//! ship at least two: a static-fixture producer (replay recorded
//! outputs while iterating on the scoring rubric) and a live
//! producer that routes through the A2 capability graph.
//!
//! ## Error semantics
//!
//! - Producer error → [`EvalCaseResult`] with `error = Some(msg)`,
//!   `candidate_output = ""`, `score = None`. The scorer is **not**
//!   called for a producer failure.
//! - Scorer error → [`EvalCaseResult`] with `error = Some(msg)`,
//!   `candidate_output` populated (the producer succeeded),
//!   `score = None`.
//! - Success → [`EvalCaseResult`] with `error = None`,
//!   `candidate_output` populated, `score = Some(_)`.
//!
//! The runner never short-circuits the loop on a single failure;
//! every case in the input produces exactly one result. This keeps
//! the run record complete for telemetry even when individual cases
//! fail transiently.
//!
//! ## Timing
//!
//! Per-case `timing_ms` covers producer + scorer wall-clock via
//! [`std::time::Instant`]. The run-level `started_at` /
//! `completed_at` come from the caller-supplied clock closure
//! (typically `chrono::Utc::now`) — separate from the
//! monotonic-Instant timing so a daylight-savings shift or NTP
//! correction during a run doesn't produce negative durations.

use std::time::Instant;

use chrono::{DateTime, Utc};

use sentinel_domain::eval::{
    EvalCase, EvalCaseId, EvalCaseResult, EvalRunId, EvalRunResult,
};
use sentinel_domain::ports::EvalScorerPort;

/// Execute the benchmark runner over a batch of cases.
///
/// `produce_candidate` is invoked once per case to obtain the
/// agent-under-test's output. Returning `Err(msg)` records a producer-
/// side failure for that case; the scorer is skipped and the loop
/// proceeds.
///
/// `scorer` judges every successfully-produced output. Scorer errors
/// also surface as per-case failures without aborting the run.
///
/// `clock` supplies the wall-clock timestamps. In production callers
/// pass `chrono::Utc::now`; tests pass a deterministic counter.
pub fn execute_run<F, S, C>(
    run_id: EvalRunId,
    cases: &[EvalCase],
    mut produce_candidate: F,
    scorer: &S,
    mut clock: C,
) -> EvalRunResult
where
    F: FnMut(&EvalCase) -> Result<String, String>,
    S: EvalScorerPort,
    C: FnMut() -> DateTime<Utc>,
{
    let started_at = clock();
    let mut case_results: Vec<EvalCaseResult> = Vec::with_capacity(cases.len());

    for case in cases {
        let case_started = Instant::now();
        let case_result = score_one_case(case, &mut produce_candidate, scorer, &run_id);
        #[allow(clippy::cast_possible_truncation)]
        let timing_ms = case_started.elapsed().as_millis() as u64;
        let completed_at = clock();
        case_results.push(EvalCaseResult {
            case_id: case.case_id.clone(),
            run_id: run_id.clone(),
            candidate_output: case_result.candidate_output,
            score: case_result.score,
            timing_ms,
            completed_at,
            error: case_result.error,
        });
    }

    let completed_at = clock();
    EvalRunResult {
        run_id,
        started_at,
        completed_at,
        case_results,
    }
}

/// Internal carrier shaped for the inner per-case fn so `execute_run`
/// can attach the timing + `completed_at` without that fn knowing
/// about either.
struct OneCaseOutcome {
    candidate_output: String,
    score: Option<sentinel_domain::eval::EvalScore>,
    error: Option<String>,
}

fn score_one_case<F, S>(
    case: &EvalCase,
    produce_candidate: &mut F,
    scorer: &S,
    run_id: &EvalRunId,
) -> OneCaseOutcome
where
    F: FnMut(&EvalCase) -> Result<String, String>,
    S: EvalScorerPort,
{
    match produce_candidate(case) {
        Err(err) => OneCaseOutcome {
            candidate_output: String::new(),
            score: None,
            error: Some(format!("producer error: {err}")),
        },
        Ok(candidate_output) => match scorer.score(case, &candidate_output, run_id) {
            Err(err) => OneCaseOutcome {
                candidate_output,
                score: None,
                error: Some(format!("scorer error: {err}")),
            },
            Ok(score) => OneCaseOutcome {
                candidate_output,
                score: Some(score),
                error: None,
            },
        },
    }
}

/// Convenience: build a fixed-output producer that looks `case_id`
/// up in a map.
///
/// Cases not present in the map produce a "no candidate recorded"
/// error. Useful for replay-style runs (rubric iteration against
/// frozen outputs) and for tests.
pub fn static_candidate_producer<H>(
    outputs: std::collections::HashMap<EvalCaseId, String, H>,
) -> impl FnMut(&EvalCase) -> Result<String, String>
where
    H: std::hash::BuildHasher,
{
    move |case: &EvalCase| {
        outputs.get(&case.case_id).cloned().ok_or_else(|| {
            format!(
                "no candidate output recorded for case_id={}",
                case.case_id.as_str()
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use sentinel_domain::eval::{
        CaseProvenance, EvalAxis, EvalAxisScore, EvalCase, EvalCaseId, EvalRunId, EvalScore,
        GoldArtifact, ScoringRubric, SourceCorpus,
    };
    use sentinel_domain::ports::{EvalScorerError, EvalScorerPort};
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn make_case(id: &str) -> EvalCase {
        EvalCase {
            case_id: EvalCaseId::new(id).unwrap(),
            stakeholder_brief: format!("brief for {id}"),
            source_corpus: SourceCorpus::Public {
                url: "https://example.com".to_string(),
                license: "CC-BY-4.0".to_string(),
            },
            gold_artifact: Some(GoldArtifact {
                text: "gold".to_string(),
                author: "tester".to_string(),
                content_hash: None,
            }),
            gold_outcomes: None,
            scoring_rubric: ScoringRubric::ba_default(),
            provenance: CaseProvenance {
                contributor: "tester".to_string(),
                license: "CC-BY-4.0".to_string(),
                is_private_test: false,
            },
        }
    }

    /// Test scorer: returns a uniform score across all axes equal to
    /// `score_value`. Can be configured to fail on specific `case_id`s.
    struct StubScorer {
        score_value: f32,
        fail_on: HashMap<String, EvalScorerError>,
    }

    impl StubScorer {
        fn always(score_value: f32) -> Self {
            Self {
                score_value,
                fail_on: HashMap::new(),
            }
        }

        fn with_failures(mut self, failures: Vec<(&str, EvalScorerError)>) -> Self {
            for (id, err) in failures {
                self.fail_on.insert(id.to_string(), err);
            }
            self
        }
    }

    impl EvalScorerPort for StubScorer {
        fn score(
            &self,
            case: &EvalCase,
            _candidate_output: &str,
            run_id: &EvalRunId,
        ) -> Result<EvalScore, EvalScorerError> {
            if let Some(err) = self.fail_on.get(case.case_id.as_str()) {
                return Err(err.clone());
            }
            let axis_scores = vec![
                EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, self.score_value, 1.0),
                EvalAxisScore::new(EvalAxis::RequirementsCoverage, self.score_value, 1.0),
                EvalAxisScore::new(EvalAxis::AlternativesSeriousness, self.score_value, 1.0),
                EvalAxisScore::new(EvalAxis::TonalCalibration, self.score_value, 1.0),
                EvalAxisScore::new(EvalAxis::OutcomeRealism, self.score_value, 2.0),
                EvalAxisScore::new(EvalAxis::StakeholderFit, self.score_value, 1.0),
            ];
            Ok(EvalScore::new(
                case.case_id.clone(),
                run_id.clone(),
                axis_scores,
                &case.scoring_rubric,
            ))
        }
    }

    fn tick_clock(start_secs: i64) -> impl FnMut() -> DateTime<Utc> {
        let counter = RefCell::new(0i64);
        move || {
            let n = *counter.borrow();
            *counter.borrow_mut() += 1;
            Utc.timestamp_opt(start_secs + n, 0).unwrap()
        }
    }

    #[test]
    fn run_with_all_successful_cases_aggregates_scores() {
        let cases = vec![make_case("c1"), make_case("c2"), make_case("c3")];
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        for c in &cases {
            outputs.insert(c.case_id.clone(), format!("output for {}", c.case_id.as_str()));
        }
        let producer = static_candidate_producer(outputs);
        let scorer = StubScorer::always(0.75);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        assert_eq!(run.case_results.len(), 3);
        assert_eq!(run.successful_case_count(), 3);
        assert_eq!(run.errored_case_count(), 0);
        let mean = run.mean_composite().expect("at least one successful case");
        assert!((mean - 0.75).abs() < 1e-3, "got {mean}");
        for cr in &run.case_results {
            assert!(cr.candidate_output.starts_with("output for"));
            assert!(cr.error.is_none());
            assert!(cr.score.is_some());
        }
    }

    #[test]
    fn run_with_missing_candidate_records_producer_error() {
        let cases = vec![make_case("c1"), make_case("c2")];
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        outputs.insert(cases[0].case_id.clone(), "only c1 has output".to_string());
        let producer = static_candidate_producer(outputs);
        let scorer = StubScorer::always(0.6);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        assert_eq!(run.case_results.len(), 2);
        assert_eq!(run.successful_case_count(), 1);
        assert_eq!(run.errored_case_count(), 1);

        let c1 = &run.case_results[0];
        assert!(c1.is_success());
        assert!(c1.score.is_some());

        let c2 = &run.case_results[1];
        assert!(c2.is_error());
        assert!(c2.score.is_none());
        assert!(c2.candidate_output.is_empty());
        let err = c2.error.as_ref().unwrap();
        assert!(err.contains("producer error"), "got {err}");
        assert!(err.contains("c2"), "should name the case_id; got {err}");
    }

    #[test]
    fn run_with_scorer_failure_records_scorer_error_with_candidate() {
        let cases = vec![make_case("c1"), make_case("c2")];
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        for c in &cases {
            outputs.insert(c.case_id.clone(), format!("output {}", c.case_id.as_str()));
        }
        let producer = static_candidate_producer(outputs);
        let scorer = StubScorer::always(0.5)
            .with_failures(vec![("c2", EvalScorerError::Backend("rate limit".into()))]);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        assert_eq!(run.successful_case_count(), 1);
        assert_eq!(run.errored_case_count(), 1);

        let c2 = &run.case_results[1];
        assert!(c2.is_error());
        assert_eq!(c2.candidate_output, "output c2");
        let err = c2.error.as_ref().unwrap();
        assert!(err.contains("scorer error"), "got {err}");
        assert!(err.contains("rate limit"), "got {err}");
    }

    #[test]
    fn run_with_empty_input_yields_empty_run() {
        let cases: Vec<EvalCase> = vec![];
        let producer = static_candidate_producer(HashMap::new());
        let scorer = StubScorer::always(0.5);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        assert!(run.case_results.is_empty());
        assert!(run.mean_composite().is_none());
        // started_at < completed_at because tick_clock advances on each call.
        assert!(run.started_at < run.completed_at);
    }

    #[test]
    fn run_timestamps_use_clock_in_call_order() {
        let cases = vec![make_case("c1")];
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        outputs.insert(cases[0].case_id.clone(), "out".to_string());
        let producer = static_candidate_producer(outputs);
        let scorer = StubScorer::always(0.5);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        // 3 clock calls total: started_at, case[0].completed_at, run.completed_at.
        // tick_clock returns successive seconds 1700000000, 1700000001, 1700000002.
        assert_eq!(
            run.started_at,
            Utc.timestamp_opt(1_700_000_000, 0).unwrap()
        );
        assert_eq!(
            run.case_results[0].completed_at,
            Utc.timestamp_opt(1_700_000_001, 0).unwrap()
        );
        assert_eq!(
            run.completed_at,
            Utc.timestamp_opt(1_700_000_002, 0).unwrap()
        );
    }

    #[test]
    fn run_preserves_case_order_in_results() {
        let cases = vec![make_case("zeta"), make_case("alpha"), make_case("middle")];
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        for c in &cases {
            outputs.insert(c.case_id.clone(), c.case_id.as_str().to_string());
        }
        let producer = static_candidate_producer(outputs);
        let scorer = StubScorer::always(0.5);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        let ids: Vec<&str> = run
            .case_results
            .iter()
            .map(|cr| cr.case_id.as_str())
            .collect();
        assert_eq!(ids, vec!["zeta", "alpha", "middle"]);
    }

    #[test]
    fn run_handles_mixed_producer_and_scorer_failures() {
        let cases = vec![
            make_case("ok"),
            make_case("producer-fails"),
            make_case("scorer-fails"),
        ];
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        outputs.insert(cases[0].case_id.clone(), "ok-output".to_string());
        outputs.insert(cases[2].case_id.clone(), "scorer-fails-output".to_string());
        // "producer-fails" deliberately not in the map.
        let producer = static_candidate_producer(outputs);
        let scorer = StubScorer::always(0.9).with_failures(vec![(
            "scorer-fails",
            EvalScorerError::Malformed("bad json".into()),
        )]);

        let run = execute_run(
            EvalRunId::new("r1").unwrap(),
            &cases,
            producer,
            &scorer,
            tick_clock(1_700_000_000),
        );

        assert_eq!(run.successful_case_count(), 1);
        assert_eq!(run.errored_case_count(), 2);

        let pf = &run.case_results[1];
        assert!(pf.is_error());
        assert!(pf.candidate_output.is_empty());
        assert!(pf.error.as_ref().unwrap().contains("producer"));

        let sf = &run.case_results[2];
        assert!(sf.is_error());
        assert_eq!(sf.candidate_output, "scorer-fails-output");
        assert!(sf.error.as_ref().unwrap().contains("scorer"));
        assert!(sf.error.as_ref().unwrap().contains("bad json"));
    }

    #[test]
    fn static_candidate_producer_returns_recorded_output() {
        let mut outputs: HashMap<EvalCaseId, String> = HashMap::new();
        outputs.insert(EvalCaseId::new("c1").unwrap(), "recorded".to_string());
        let mut producer = static_candidate_producer(outputs);
        let case = make_case("c1");
        assert_eq!(producer(&case).unwrap(), "recorded");
    }

    #[test]
    fn static_candidate_producer_errors_on_missing() {
        let outputs: HashMap<EvalCaseId, String> = HashMap::new();
        let mut producer = static_candidate_producer(outputs);
        let case = make_case("missing");
        let err = producer(&case).expect_err("should error on missing");
        assert!(err.contains("missing"), "error should name case_id; got {err}");
    }
}
