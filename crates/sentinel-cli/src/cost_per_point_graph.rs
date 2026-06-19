use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::cost_per_point::CostPerPointReport;
use sentinel_infrastructure::cost_per_point_graph::{
    build_cost_per_point_graph, cost_per_point_decision_label, run_cost_per_point_decision_report,
    CostPerPointState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CostPerPointGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_cost_per_point_graph_audit(
    report: &CostPerPointReport,
    graph_jsonl: &Path,
) -> Result<CostPerPointGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create cost-per-point graph dir {}", parent.display()))?;
    }
    let graph = build_cost_per_point_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build cost-per-point graph: {e}"))?;
    let state = CostPerPointState::from_report(report);
    let run = run_cost_per_point_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("cost-per-point graph failed: {e}"))?;
    let authorization = run
        .cost_per_point_authorization()
        .map_err(|e| anyhow::anyhow!("cost-per-point graph authorization failed: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!("cost-per-point graph produced no authorization checkpoint")
        })?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = cost_per_point_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "cost_per_point",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create cost-per-point graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row).with_context(|| {
        format!(
            "write cost-per-point graph row to {}",
            graph_jsonl.display()
        )
    })?;
    writer.write_all(b"\n").with_context(|| {
        format!(
            "terminate cost-per-point graph row in {}",
            graph_jsonl.display()
        )
    })?;
    writer
        .flush()
        .with_context(|| format!("flush cost-per-point graph {}", graph_jsonl.display()))?;
    Ok(CostPerPointGraphAudit {
        workflow_authority: "langgraph",
        graph: "cost_per_point",
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
    use sentinel_application::cost_per_point::BucketStats;
    use std::collections::BTreeMap;

    fn bucket(n: usize, cost_p50: f64) -> BucketStats {
        BucketStats {
            n,
            cost_p25: cost_p50,
            cost_p50,
            cost_p75: cost_p50,
            cost_p90: cost_p50,
            tokens_p25: 1_000.0,
            tokens_p50: 1_000.0,
            tokens_p75: 1_000.0,
            tokens_p90: 1_000.0,
        }
    }

    fn report() -> CostPerPointReport {
        let mut buckets = BTreeMap::new();
        buckets.insert(2, bucket(5, 1.0));
        buckets.insert(8, bucket(5, 4.0));
        CostPerPointReport {
            tickets_analyzed: 10,
            tickets_with_estimate: 10,
            buckets,
            drift_ratio_high_vs_low: Some(4.0),
            drift_alarm: false,
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn cost_per_point_audit_runs_report_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("cost-per-point-summary.graph-runs.jsonl");

        let audit = run_cost_per_point_graph_audit(&report(), &graph_jsonl)
            .await
            .expect("cost-per-point graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "cost_per_point");
        assert_eq!(audit.decision, "healthy-curve");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "cost_per_point");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("cost-per-point graph jsonl")
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
