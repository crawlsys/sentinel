use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::pr_review::PrReviewReport;
use sentinel_infrastructure::pr_review_graph::{
    build_pr_review_graph, pr_review_decision_label, run_pr_review_decision_report, PrReviewState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PrReviewGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: String,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_pr_review_graph_audit(
    report: &PrReviewReport,
    graph_jsonl: &Path,
) -> Result<PrReviewGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create PR review graph dir {}", parent.display()))?;
    }
    let graph = build_pr_review_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build PR review graph: {e}"))?;
    let state = PrReviewState::from_report(report);
    let run = run_pr_review_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("PR review graph failed: {e}"))?;
    let authorization = run
        .pr_review_authorization()
        .map_err(|e| anyhow::anyhow!("PR review graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("PR review graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = authorization.checkpoint_ref();
    let decision = pr_review_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "pr_review",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create PR review graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write PR review graph row to {}", graph_jsonl.display()))?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("terminate PR review graph row in {}", graph_jsonl.display()))?;
    writer
        .flush()
        .with_context(|| format!("flush PR review graph {}", graph_jsonl.display()))?;
    Ok(PrReviewGraphAudit {
        workflow_authority: "langgraph",
        graph: "pr_review",
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
    use sentinel_application::pr_review::PerRepo;

    fn report() -> PrReviewReport {
        PrReviewReport {
            repos: vec!["legatus-ai/sentinel".to_string()],
            window_days: 30,
            total_prs: 4,
            avg_comments_per_pr: 3.0,
            p50_time_to_first_review_hours: 4.0,
            p90_time_to_first_review_hours: 24.0,
            codex_findings_total: 4,
            coderabbit_findings_total: 0,
            human_review_pct: 75.0,
            per_repo: vec![PerRepo {
                repo: "legatus-ai/sentinel".to_string(),
                prs: 4,
                avg_comments: 3.0,
                p50_ttfr_hours: 4.0,
                p90_ttfr_hours: 24.0,
                codex_findings: 4,
                coderabbit_findings: 0,
                human_review_pct: 75.0,
            }],
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pr_review_audit_runs_report_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("pr-review-summary.graph-runs.jsonl");

        let audit = run_pr_review_graph_audit(&report(), &graph_jsonl)
            .await
            .expect("PR review graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "pr_review");
        assert_eq!(audit.decision, "healthy-review-loop");
        assert!(audit.authorization_checkpoint.contains('#'));
        assert_eq!(audit.run["topology"]["graph"], "pr_review");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("PR review graph jsonl")
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
