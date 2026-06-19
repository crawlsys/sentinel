use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use sentinel_application::linear_health_score::HealthSummary;
use sentinel_application::mcp_handler::LinearHealthGraphAudit;
use sentinel_infrastructure::linear_health_graph::{
    build_linear_health_graph, linear_health_decision_label, run_linear_health_decision_report,
    LinearHealthState,
};

pub(crate) async fn run_linear_health_graph_audit(
    summary: &HealthSummary,
    graph_jsonl: &Path,
) -> Result<LinearHealthGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create Linear health graph dir {}", parent.display()))?;
    }
    let graph = build_linear_health_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build Linear health graph: {e}"))?;
    let state = LinearHealthState::from_summary(summary);
    let run = run_linear_health_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("Linear health graph failed: {e}"))?;
    let authorization = run
        .health_authorization()
        .map_err(|e| anyhow::anyhow!("linear health graph authorization failed: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!("Linear health graph produced no authorization checkpoint")
        })?;
    let authorization_checkpoint = authorization.checkpoint_ref();
    let decision = linear_health_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "linear_health",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create Linear health graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write Linear health graph row to {}", graph_jsonl.display()))?;
    writer.write_all(b"\n").with_context(|| {
        format!(
            "terminate Linear health graph row in {}",
            graph_jsonl.display()
        )
    })?;
    writer
        .flush()
        .with_context(|| format!("flush Linear health graph {}", graph_jsonl.display()))?;
    Ok(LinearHealthGraphAudit {
        workflow_authority: "langgraph",
        graph: "linear_health",
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

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn health_audit_runs_summary_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("linear-health.graph-runs.jsonl");
        let summary = HealthSummary {
            issues_total: 2,
            total_score: 88,
            hygiene_score: 28.0,
            structure_score: 20.0,
            data_quality_score: 15.0,
            flow_score: 25.0,
            qa_congestion_fraction: 0.1,
            qa_failed_count: 0,
            grade: "healthy".to_string(),
        };

        let audit = run_linear_health_graph_audit(&summary, &graph_jsonl)
            .await
            .expect("Linear health graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "linear_health");
        assert_eq!(audit.decision, "healthy");
        assert!(audit.authorization_checkpoint.contains('#'));
        assert_eq!(audit.run["topology"]["graph"], "linear_health");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("health graph jsonl")
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
