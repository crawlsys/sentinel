use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_domain::eval::EvalRunResult;
use sentinel_infrastructure::eval_graph::{
    build_eval_graph, eval_decision_label, run_eval_decision_report, EvalRunState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EvalGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: String,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_eval_graph_audit(
    run_result: &EvalRunResult,
    graph_jsonl: &Path,
) -> Result<EvalGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create eval graph dir {}", parent.display()))?;
    }
    let graph = build_eval_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build eval graph: {e}"))?;
    let state = EvalRunState::from_run(run_result);
    let run = run_eval_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("eval graph failed: {e}"))?;
    let authorization = run
        .eval_authorization()
        .map_err(|e| anyhow::anyhow!("eval graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("eval graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = authorization.checkpoint_ref();
    let decision = eval_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "eval",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create eval graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write eval graph row to {}", graph_jsonl.display()))?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("terminate eval graph row in {}", graph_jsonl.display()))?;
    writer
        .flush()
        .with_context(|| format!("flush eval graph {}", graph_jsonl.display()))?;
    Ok(EvalGraphAudit {
        workflow_authority: "langgraph",
        graph: "eval",
        graph_runs_path: graph_jsonl.to_path_buf(),
        decision,
        authorization_checkpoint,
        thread_id,
        run: row["run"].clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::eval::{
        EvalAxis, EvalAxisScore, EvalCaseId, EvalCaseResult, EvalRunId, EvalScore, ScoringRubric,
    };

    fn run_result() -> EvalRunResult {
        let run_id = EvalRunId::new("eval-cli-graph").unwrap();
        let rubric = ScoringRubric::ba_default();
        let axis_scores = vec![
            EvalAxisScore::new(EvalAxis::CitationDensityAccuracy, 0.90, 1.0),
            EvalAxisScore::new(EvalAxis::RequirementsCoverage, 0.90, 1.0),
            EvalAxisScore::new(EvalAxis::AlternativesSeriousness, 0.90, 1.0),
            EvalAxisScore::new(EvalAxis::TonalCalibration, 0.90, 1.0),
            EvalAxisScore::new(EvalAxis::OutcomeRealism, 0.90, 2.0),
            EvalAxisScore::new(EvalAxis::StakeholderFit, 0.90, 1.0),
        ];
        let case_id = EvalCaseId::new("case-1").unwrap();
        let score = EvalScore::new(case_id.clone(), run_id.clone(), axis_scores, &rubric);
        EvalRunResult {
            run_id: run_id.clone(),
            started_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            completed_at: Utc.timestamp_opt(1_700_000_010, 0).unwrap(),
            case_results: vec![EvalCaseResult {
                case_id,
                run_id,
                candidate_output: "candidate".to_string(),
                score: Some(score),
                timing_ms: 10,
                completed_at: Utc.timestamp_opt(1_700_000_005, 0).unwrap(),
                error: None,
            }],
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn eval_audit_runs_result_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("eval.graph-runs.jsonl");

        let audit = run_eval_graph_audit(&run_result(), &graph_jsonl)
            .await
            .expect("eval graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "eval");
        assert_eq!(audit.decision, "strong");
        assert!(audit.authorization_checkpoint.contains('#'));
        assert_eq!(audit.run["topology"]["graph"], "eval");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("eval graph jsonl")
            .contains("\"workflow_authority\":\"langgraph\""));

        match previous_home {
            Some(value) => std::env::set_var("SENTINEL_HOME", value),
            None => std::env::remove_var("SENTINEL_HOME"),
        }
        match previous_backend {
            Some(value) => std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", value),
            None => std::env::remove_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER"),
        }
    }
}
