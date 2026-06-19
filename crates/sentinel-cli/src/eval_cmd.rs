//! `sentinel eval` — A12 external-benchmark CLI.
//!
//! Phase 2 ships the `list` subcommand. Phase 3e adds `run`: load
//! cases, replay recorded candidate outputs through an
//! [`EvalScorerPort`], persist the run via [`EvalRunStorePort`], then
//! authorize the aggregate verdict through a durable LangGraph.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use sentinel_application::eval_run::{execute_run, static_candidate_producer};
use sentinel_domain::eval::{EvalCaseId, EvalRunId, EvalRunResult};
use sentinel_domain::ports::{EvalRunStorePort, EvalScorerPort};
use sentinel_infrastructure::eval_corpus::{CorpusEntry, FilesystemEvalCorpus};
use sentinel_infrastructure::eval_run_store::FilesystemEvalRunStore;
use sentinel_infrastructure::eval_scorer::LlmEvalScorer;

/// `sentinel eval list` — render every `case_id` in the BA-Eval
/// corpus. Output includes both the public and private test splits,
/// with the `is_private_test` flag clearly marked so operators see
/// which cases are held-back per spec §3.4.
pub fn list(json: bool, dir_override: Option<String>) -> Result<()> {
    let corpus = build_corpus(dir_override)?;
    let entries = corpus.list_case_ids()?;
    if json {
        render_json(&entries);
    } else {
        render_table(corpus.base_dir(), &entries);
    }
    Ok(())
}

fn build_corpus(dir_override: Option<String>) -> Result<FilesystemEvalCorpus> {
    dir_override.map_or_else(FilesystemEvalCorpus::with_default_path, |dir| {
        Ok(FilesystemEvalCorpus::at_dir(PathBuf::from(dir)))
    })
}

fn render_json(entries: &[CorpusEntry]) {
    let payload: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "case_id": e.case_id.as_str(),
                "is_private_test": e.is_private_test,
            })
        })
        .collect();
    let out = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string());
    println!("{out}");
}

fn render_table(base_dir: &std::path::Path, entries: &[CorpusEntry]) {
    if entries.is_empty() {
        println!("No cases registered under {}", base_dir.display());
        println!();
        println!(
            "To populate the corpus, drop EvalCase JSON files into:\n  \
             {}/cases/{{case_id}}.json\n  \
             {}/private-test-split/{{case_id}}.json",
            base_dir.display(),
            base_dir.display(),
        );
        return;
    }
    let public_count = entries.iter().filter(|e| !e.is_private_test).count();
    let private_count = entries.len() - public_count;
    println!(
        "BA-Eval corpus at {} — {} cases ({} public, {} private)",
        base_dir.display(),
        entries.len(),
        public_count,
        private_count,
    );
    println!();
    println!("  {:<40}  split", "case_id");
    println!("  {:-<40}  --------", "");
    for e in entries {
        let split = if e.is_private_test {
            "private"
        } else {
            "public"
        };
        println!("  {:<40}  {split}", e.case_id.as_str());
    }
}

// ---------------------------------------------------------------------------
// `sentinel eval run` — Phase 3e benchmark runner CLI.
// ---------------------------------------------------------------------------

/// Arguments for `sentinel eval run` — gathered into a struct so the
/// clap definition + the test seam share one shape.
pub struct RunArgs {
    pub run_id: String,
    /// Path to a JSON file mapping `case_id -> candidate_output`.
    /// Eval scoring operates on explicit candidate artifacts; agent
    /// dispatch belongs to the upstream orchestration path.
    pub candidates_path: String,
    /// Optional filter: if non-empty, only these `case_id`s run.
    pub case_ids: Vec<String>,
    pub corpus_dir: Option<String>,
    pub runs_dir: Option<String>,
    pub json: bool,
}

/// `sentinel eval run` — production entry point. Builds an
/// [`LlmEvalScorer`] from env, executes the run, and writes a durable
/// LangGraph audit row for the aggregate benchmark verdict.
pub async fn run(mut args: RunArgs) -> Result<()> {
    let scorer =
        LlmEvalScorer::from_env().context("failed to build eval scorer from environment")?;
    let corpus = build_corpus(args.corpus_dir.take())?;
    let store = build_run_store(args.runs_dir.take())?;
    let run = run_with(&args, &corpus, &store, &scorer)?;
    let graph_runs = store
        .base_dir()
        .join(format!("{}.graph-runs.jsonl", run.run_id.as_str()));
    let graph_audit = crate::eval_graph::run_eval_graph_audit(&run, &graph_runs).await?;
    if args.json {
        render_run_json(&run, &graph_audit);
    } else {
        render_run_summary(&run);
        render_graph_summary(&graph_audit);
    }
    Ok(())
}

/// Test seam — accepts pre-built corpus / store / scorer so tests
/// can inject tempdir-scoped adapters without env vars.
/// Generic over the store + scorer types (not `&dyn`) so the
/// downstream call to [`execute_run`] stays on the static-dispatch
/// path; `&dyn` would require the use case to relax its `Sized`
/// bound, which adds noise for a single caller.
pub fn run_with<St, Sc>(
    args: &RunArgs,
    corpus: &FilesystemEvalCorpus,
    store: &St,
    scorer: &Sc,
) -> Result<EvalRunResult>
where
    St: EvalRunStorePort,
    Sc: EvalScorerPort,
{
    let run_id = EvalRunId::new(&args.run_id)
        .map_err(|e| anyhow::anyhow!("invalid run id {:?}: {e}", args.run_id))?;

    let candidates = load_candidates(Path::new(&args.candidates_path))?;
    let cases = load_cases(corpus, &args.case_ids)?;

    if cases.is_empty() {
        anyhow::bail!(
            "no cases to run; corpus at {} has no matching cases",
            corpus.base_dir().display()
        );
    }

    let run = execute_run(
        run_id,
        &cases,
        static_candidate_producer(candidates),
        scorer,
        chrono::Utc::now,
    );

    store
        .save(&run)
        .map_err(|e| anyhow::anyhow!("failed to save run: {e}"))?;

    Ok(run)
}

fn build_run_store(runs_dir: Option<String>) -> Result<FilesystemEvalRunStore> {
    runs_dir.map_or_else(FilesystemEvalRunStore::with_default_path, |dir| {
        Ok(FilesystemEvalRunStore::at_dir(PathBuf::from(dir)))
    })
}

fn load_candidates(path: &Path) -> Result<HashMap<EvalCaseId, String>> {
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read candidates file {}", path.display()))?;
    let raw: HashMap<String, String> = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "candidates file {} must be a JSON object mapping case_id -> output",
            path.display()
        )
    })?;
    let mut out: HashMap<EvalCaseId, String> = HashMap::new();
    for (k, v) in raw {
        let id = EvalCaseId::new(&k)
            .map_err(|e| anyhow::anyhow!("invalid case_id {k:?} in candidates file: {e}"))?;
        out.insert(id, v);
    }
    Ok(out)
}

fn load_cases(
    corpus: &FilesystemEvalCorpus,
    filter_ids: &[String],
) -> Result<Vec<sentinel_domain::eval::EvalCase>> {
    let entries = corpus.list_case_ids()?;
    let mut cases = Vec::new();
    for entry in entries {
        if !filter_ids.is_empty() && !filter_ids.iter().any(|f| f == entry.case_id.as_str()) {
            continue;
        }
        let case = corpus.load_case(&entry.case_id)?;
        cases.push(case);
    }
    Ok(cases)
}

fn render_run_json(run: &EvalRunResult, graph_audit: &crate::eval_graph::EvalGraphAudit) {
    let out = serde_json::to_string_pretty(&serde_json::json!({
        "workflow_authority": "langgraph",
        "run": run,
        "graph_audit": graph_audit,
    }))
    .unwrap_or_else(|_| "{}".to_string());
    println!("{out}");
}

fn render_graph_summary(graph_audit: &crate::eval_graph::EvalGraphAudit) {
    println!(
        "  graph:     {} ({})",
        graph_audit.decision,
        graph_audit.authorization_checkpoint.as_str()
    );
    println!("  graph log: {}", graph_audit.graph_runs_path.display());
}

fn render_run_summary(run: &EvalRunResult) {
    println!("Run {} persisted", run.run_id.as_str());
    println!(
        "  started:   {}\n  completed: {}",
        run.started_at, run.completed_at,
    );
    println!(
        "  cases:     {} ({} successful, {} errored)",
        run.case_results.len(),
        run.successful_case_count(),
        run.errored_case_count(),
    );
    if let Some(mean) = run.mean_composite() {
        println!("  composite: {mean:.3} (mean across successful cases)");
    } else {
        println!("  composite: <no successful cases>");
    }
    let by_axis = run.mean_per_axis();
    if !by_axis.is_empty() {
        println!();
        println!("  Per-axis mean (raw):");
        for (axis, mean) in by_axis {
            println!("    {:<32} {mean:.3}", axis.key());
        }
    }
    let errored: Vec<&sentinel_domain::eval::EvalCaseResult> =
        run.case_results.iter().filter(|c| c.is_error()).collect();
    if !errored.is_empty() {
        println!();
        println!("  Errored cases:");
        for c in errored {
            let err = c.error.as_deref().unwrap_or("?");
            println!("    {}  {err}", c.case_id.as_str());
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::eval::{
        CaseProvenance, EvalCase, EvalCaseId, GoldArtifact, ScoringRubric, SourceCorpus,
    };
    use std::fs;
    use tempfile::TempDir;

    fn write_case(corpus_dir: &std::path::Path, case_id: &str, is_private: bool) {
        let split_dir = if is_private {
            corpus_dir.join("private-test-split")
        } else {
            corpus_dir.join("cases")
        };
        fs::create_dir_all(&split_dir).unwrap();
        let case = EvalCase {
            case_id: EvalCaseId::new(case_id).unwrap(),
            stakeholder_brief: "test".into(),
            source_corpus: SourceCorpus::Public {
                url: "https://example.com".into(),
                license: "CC-BY-4.0".into(),
            },
            gold_artifact: Some(GoldArtifact {
                text: "gold".into(),
                author: "tester".into(),
                content_hash: None,
            }),
            gold_outcomes: None,
            scoring_rubric: ScoringRubric::ba_default(),
            provenance: CaseProvenance {
                contributor: "tester".into(),
                license: "CC-BY-4.0".into(),
                is_private_test: is_private,
            },
        };
        let path = split_dir.join(format!("{case_id}.json"));
        fs::write(&path, serde_json::to_string(&case).unwrap()).unwrap();
    }

    #[test]
    fn list_with_dir_override_succeeds_on_empty_corpus() {
        let dir = TempDir::new().unwrap();
        let result = list(false, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_populated_public_corpus_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "case-A", false);
        write_case(dir.path(), "case-B", false);
        let result = list(false, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_private_split_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "case-X", true);
        let result = list(false, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn list_json_mode_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "case-A", false);
        write_case(dir.path(), "case-X", true);
        let result = list(true, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn build_corpus_with_override_uses_override_path() {
        let dir = TempDir::new().unwrap();
        let corpus = build_corpus(Some(dir.path().to_string_lossy().into_owned())).unwrap();
        assert_eq!(corpus.base_dir(), dir.path());
    }

    #[test]
    fn build_corpus_without_override_uses_default_path() {
        let corpus = build_corpus(None).unwrap();
        let p = corpus.base_dir().display().to_string();
        assert!(p.contains(".claude") && p.contains("ba-corpus"));
    }

    // -----------------------------------------------------------------------
    // run / run_with tests
    // -----------------------------------------------------------------------

    use sentinel_domain::eval::{EvalAxis, EvalAxisScore, EvalRunId, EvalScore};
    use sentinel_domain::ports::{EvalRunStoreError, EvalScorerError, EvalScorerPort};
    use std::sync::Mutex;

    /// Test scorer: always returns uniform `score_value` across all axes.
    struct StubScorer {
        score_value: f32,
    }

    impl EvalScorerPort for StubScorer {
        fn score(
            &self,
            case: &EvalCase,
            _candidate_output: &str,
            run_id: &EvalRunId,
        ) -> Result<EvalScore, EvalScorerError> {
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

    /// Recording store: keeps the most recent save in memory so tests
    /// can assert on the persisted shape.
    struct RecordingStore {
        saved: Mutex<Option<EvalRunResult>>,
    }

    impl RecordingStore {
        fn new() -> Self {
            Self {
                saved: Mutex::new(None),
            }
        }
        fn last_saved(&self) -> Option<EvalRunResult> {
            self.saved.lock().unwrap().clone()
        }
    }

    impl EvalRunStorePort for RecordingStore {
        fn save(&self, run: &EvalRunResult) -> Result<(), EvalRunStoreError> {
            *self.saved.lock().unwrap() = Some(run.clone());
            Ok(())
        }
        fn load(&self, _run_id: &EvalRunId) -> Result<Option<EvalRunResult>, EvalRunStoreError> {
            Ok(self.saved.lock().unwrap().clone())
        }
        fn list_run_ids(&self) -> Result<Vec<EvalRunId>, EvalRunStoreError> {
            Ok(Vec::new())
        }
    }

    fn write_candidates(path: &std::path::Path, entries: &[(&str, &str)]) {
        let map: std::collections::BTreeMap<&str, &str> = entries.iter().copied().collect();
        fs::write(path, serde_json::to_string(&map).unwrap()).unwrap();
    }

    #[test]
    fn run_with_loads_cases_replays_candidates_and_persists() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "c1", false);
        write_case(dir.path(), "c2", false);
        let candidates_path = dir.path().join("candidates.json");
        write_candidates(&candidates_path, &[("c1", "output 1"), ("c2", "output 2")]);

        let corpus = FilesystemEvalCorpus::at_dir(dir.path().to_path_buf());
        let store = RecordingStore::new();
        let scorer = StubScorer { score_value: 0.7 };

        let args = RunArgs {
            run_id: "test-run-1".to_string(),
            candidates_path: candidates_path.to_string_lossy().into_owned(),
            case_ids: vec![],
            corpus_dir: None,
            runs_dir: None,
            json: false,
        };

        run_with(&args, &corpus, &store, &scorer).unwrap();
        let saved = store.last_saved().expect("run was saved");
        assert_eq!(saved.run_id.as_str(), "test-run-1");
        assert_eq!(saved.case_results.len(), 2);
        assert_eq!(saved.successful_case_count(), 2);
    }

    #[test]
    fn run_with_filters_by_case_id() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "c1", false);
        write_case(dir.path(), "c2", false);
        let candidates_path = dir.path().join("candidates.json");
        write_candidates(&candidates_path, &[("c1", "output 1"), ("c2", "output 2")]);

        let corpus = FilesystemEvalCorpus::at_dir(dir.path().to_path_buf());
        let store = RecordingStore::new();
        let scorer = StubScorer { score_value: 0.5 };

        let args = RunArgs {
            run_id: "test-run-filter".to_string(),
            candidates_path: candidates_path.to_string_lossy().into_owned(),
            case_ids: vec!["c1".to_string()],
            corpus_dir: None,
            runs_dir: None,
            json: false,
        };

        run_with(&args, &corpus, &store, &scorer).unwrap();
        let saved = store.last_saved().unwrap();
        assert_eq!(saved.case_results.len(), 1);
        assert_eq!(saved.case_results[0].case_id.as_str(), "c1");
    }

    #[test]
    fn run_with_records_producer_error_when_candidate_missing() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "c1", false);
        write_case(dir.path(), "c2", false);
        let candidates_path = dir.path().join("candidates.json");
        // c2 deliberately omitted.
        write_candidates(&candidates_path, &[("c1", "output 1")]);

        let corpus = FilesystemEvalCorpus::at_dir(dir.path().to_path_buf());
        let store = RecordingStore::new();
        let scorer = StubScorer { score_value: 0.8 };

        let args = RunArgs {
            run_id: "test-run-partial".to_string(),
            candidates_path: candidates_path.to_string_lossy().into_owned(),
            case_ids: vec![],
            corpus_dir: None,
            runs_dir: None,
            json: false,
        };

        run_with(&args, &corpus, &store, &scorer).unwrap();
        let saved = store.last_saved().unwrap();
        assert_eq!(saved.successful_case_count(), 1);
        assert_eq!(saved.errored_case_count(), 1);
    }

    #[test]
    fn run_with_errors_on_empty_corpus() {
        let dir = TempDir::new().unwrap();
        let candidates_path = dir.path().join("candidates.json");
        fs::write(&candidates_path, "{}").unwrap();

        let corpus = FilesystemEvalCorpus::at_dir(dir.path().to_path_buf());
        let store = RecordingStore::new();
        let scorer = StubScorer { score_value: 0.5 };

        let args = RunArgs {
            run_id: "test-empty".to_string(),
            candidates_path: candidates_path.to_string_lossy().into_owned(),
            case_ids: vec![],
            corpus_dir: None,
            runs_dir: None,
            json: false,
        };

        let err = run_with(&args, &corpus, &store, &scorer).unwrap_err();
        assert!(err.to_string().contains("no cases to run"));
    }

    #[test]
    fn run_with_errors_on_invalid_run_id() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "c1", false);
        let candidates_path = dir.path().join("candidates.json");
        write_candidates(&candidates_path, &[("c1", "x")]);

        let corpus = FilesystemEvalCorpus::at_dir(dir.path().to_path_buf());
        let store = RecordingStore::new();
        let scorer = StubScorer { score_value: 0.5 };

        let args = RunArgs {
            run_id: String::new(), // EvalRunId::new rejects empty
            candidates_path: candidates_path.to_string_lossy().into_owned(),
            case_ids: vec![],
            corpus_dir: None,
            runs_dir: None,
            json: false,
        };

        let err = run_with(&args, &corpus, &store, &scorer).unwrap_err();
        assert!(err.to_string().contains("invalid run id"));
    }

    #[test]
    fn load_candidates_parses_valid_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.json");
        fs::write(&path, r#"{"alpha": "out a", "beta": "out b"}"#).unwrap();
        let map = load_candidates(&path).unwrap();
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn load_candidates_errors_on_invalid_case_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.json");
        // Empty-string case_id is rejected by EvalCaseId::new.
        fs::write(&path, r#"{"": "out"}"#).unwrap();
        let err = load_candidates(&path).unwrap_err();
        assert!(err.to_string().contains("invalid case_id"));
    }

    #[test]
    fn load_candidates_errors_on_missing_file() {
        let err = load_candidates(Path::new("/nonexistent/path.json")).unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn load_candidates_errors_on_malformed_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("c.json");
        fs::write(&path, "not json").unwrap();
        let err = load_candidates(&path).unwrap_err();
        assert!(err.to_string().contains("JSON object"));
    }

    #[test]
    fn run_with_json_mode_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "c1", false);
        let candidates_path = dir.path().join("c.json");
        write_candidates(&candidates_path, &[("c1", "out")]);

        let corpus = FilesystemEvalCorpus::at_dir(dir.path().to_path_buf());
        let store = RecordingStore::new();
        let scorer = StubScorer { score_value: 0.5 };

        let args = RunArgs {
            run_id: "json-run".to_string(),
            candidates_path: candidates_path.to_string_lossy().into_owned(),
            case_ids: vec![],
            corpus_dir: None,
            runs_dir: None,
            json: true,
        };

        run_with(&args, &corpus, &store, &scorer).unwrap();
        assert!(store.last_saved().is_some());
    }

    #[test]
    fn build_run_store_with_override_uses_override_path() {
        let dir = TempDir::new().unwrap();
        let store = build_run_store(Some(dir.path().to_string_lossy().into_owned())).unwrap();
        assert_eq!(store.base_dir(), dir.path());
    }

    #[test]
    fn build_run_store_without_override_uses_default_path() {
        let store = build_run_store(None).unwrap();
        let p = store.base_dir().display().to_string();
        assert!(p.contains(".claude") && p.contains("runs"));
    }
}
