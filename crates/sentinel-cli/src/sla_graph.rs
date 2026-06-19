use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::sla::BreachesSummary;
use sentinel_infrastructure::sla_graph::{
    build_sla_graph, run_sla_decision_report, sla_decision_label, SlaState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SlaGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_sla_graph_audit(
    summary: &BreachesSummary,
    graph_jsonl: &Path,
) -> Result<SlaGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create sla graph dir {}", parent.display()))?;
    }
    let graph = build_sla_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build sla graph: {e}"))?;
    let state = SlaState::from_summary(summary);
    let run = run_sla_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("sla graph failed: {e}"))?;
    let authorization = run
        .sla_authorization()
        .map_err(|e| anyhow::anyhow!("sla graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("sla graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = sla_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "sla",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create sla graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write sla graph row to {}", graph_jsonl.display()))?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("terminate sla graph row in {}", graph_jsonl.display()))?;
    writer
        .flush()
        .with_context(|| format!("flush sla graph {}", graph_jsonl.display()))?;
    Ok(SlaGraphAudit {
        workflow_authority: "langgraph",
        graph: "sla",
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
    use sentinel_application::sla::SlaAggregate;

    fn summary() -> BreachesSummary {
        BreachesSummary {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            records_scanned: 3,
            aggregates: vec![SlaAggregate {
                sla: "P0 pickup".to_string(),
                breaches_24h: 1,
                breaches_7d: 2,
                breaches_30d: 3,
                most_recent: Some("2026-06-18T00:00:00Z".to_string()),
            }],
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sla_audit_runs_summary_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("sla-breaches-summary.graph-runs.jsonl");

        let audit = run_sla_graph_audit(&summary(), &graph_jsonl)
            .await
            .expect("sla graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "sla");
        assert_eq!(audit.decision, "active-breach");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "sla");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("sla graph jsonl")
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
