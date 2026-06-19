use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::roi::RoiReport;
use sentinel_infrastructure::roi_graph::{
    build_roi_graph, roi_decision_label, run_roi_decision_report, RoiState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RoiGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_roi_graph_audit(
    report: &RoiReport,
    graph_jsonl: &Path,
) -> Result<RoiGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create roi graph dir {}", parent.display()))?;
    }
    let graph = build_roi_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build roi graph: {e}"))?;
    let state = RoiState::from_report(report);
    let run = run_roi_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("roi graph failed: {e}"))?;
    let authorization = run
        .roi_authorization()
        .map_err(|e| anyhow::anyhow!("roi graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("roi graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = roi_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "roi",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create roi graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write roi graph row to {}", graph_jsonl.display()))?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("terminate roi graph row in {}", graph_jsonl.display()))?;
    writer
        .flush()
        .with_context(|| format!("flush roi graph {}", graph_jsonl.display()))?;
    Ok(RoiGraphAudit {
        workflow_authority: "langgraph",
        graph: "roi",
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
    use sentinel_application::roi::{human_baseline_per_point, HeadlineRoi, RoiWindow};

    fn report() -> RoiReport {
        let window = RoiWindow {
            window_days: None,
            label: "all-time".to_string(),
            tickets_shipped: 2,
            points_shipped: 5.0,
            claude_cost_usd: 10.0,
            claude_cost_per_point: 2.0,
            human_cost_usd: 1_635.0,
            roi_ratio: 163.5,
            projected_annual_savings_usd: 1_625.0,
            estimate_data_available: true,
            estimate_note: String::new(),
        };
        let headline = HeadlineRoi {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            roi_ratio: window.roi_ratio,
            claude_cost_usd_total: window.claude_cost_usd,
            human_cost_usd_total: window.human_cost_usd,
            tickets_shipped_total: window.tickets_shipped,
            projected_annual_savings_usd: window.projected_annual_savings_usd,
            claude_cost_per_point: window.claude_cost_per_point,
            human_cost_per_point: human_baseline_per_point(),
            estimate_data_available: true,
            estimate_note: String::new(),
            windows: vec![window.clone()],
        };
        RoiReport {
            windows: vec![window],
            headline: Some(headline),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn roi_audit_runs_report_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("roi-summary.graph-runs.jsonl");

        let audit = run_roi_graph_audit(&report(), &graph_jsonl)
            .await
            .expect("roi graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "roi");
        assert_eq!(audit.decision, "positive-return");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "roi");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("roi graph jsonl")
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
