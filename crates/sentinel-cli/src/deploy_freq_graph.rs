use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::deploy_freq::DeploySummary;
use sentinel_infrastructure::deploy_freq_graph::{
    build_deploy_frequency_graph, deploy_frequency_decision_label,
    run_deploy_frequency_decision_report, DeployFrequencyState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DeployFrequencyGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_deploy_frequency_graph_audit(
    summary: &DeploySummary,
    graph_jsonl: &Path,
) -> Result<DeployFrequencyGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create deploy frequency graph dir {}", parent.display()))?;
    }
    let graph = build_deploy_frequency_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build deploy frequency graph: {e}"))?;
    let state = DeployFrequencyState::from_summary(summary);
    let run = run_deploy_frequency_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("deploy frequency graph failed: {e}"))?;
    let authorization = run
        .deploy_frequency_authorization()
        .map_err(|e| anyhow::anyhow!("deploy frequency graph authorization failed: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!("deploy frequency graph produced no authorization checkpoint")
        })?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = deploy_frequency_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "deploy_frequency",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create deploy frequency graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row).with_context(|| {
        format!(
            "write deploy frequency graph row to {}",
            graph_jsonl.display()
        )
    })?;
    writer.write_all(b"\n").with_context(|| {
        format!(
            "terminate deploy frequency graph row in {}",
            graph_jsonl.display()
        )
    })?;
    writer
        .flush()
        .with_context(|| format!("flush deploy frequency graph {}", graph_jsonl.display()))?;
    Ok(DeployFrequencyGraphAudit {
        workflow_authority: "langgraph",
        graph: "deploy_frequency",
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
    use sentinel_application::deploy_freq::{DailyPoint, DoraTier, RepoEnvAggregate};

    fn summary() -> DeploySummary {
        DeploySummary {
            generated_at: "2026-06-18T00:00:00Z".to_string(),
            records_scanned: 30,
            aggregates: vec![RepoEnvAggregate {
                repo: "sentinel".to_string(),
                env: "prod".to_string(),
                deploys_7d: 7,
                deploys_30d: 30,
                rate_per_day_7d: 1.0,
                rate_per_day_30d: 1.0,
                tier: DoraTier::Elite,
                first_in_window: Some("2026-06-01T00:00:00Z".to_string()),
                last_in_window: Some("2026-06-18T00:00:00Z".to_string()),
            }],
            daily_points: vec![DailyPoint {
                date: "2026-06-18".to_string(),
                repo: "sentinel".to_string(),
                env: "prod".to_string(),
                deploys: 30,
            }],
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn deploy_frequency_audit_runs_summary_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("deploys-summary.graph-runs.jsonl");

        let audit = run_deploy_frequency_graph_audit(&summary(), &graph_jsonl)
            .await
            .expect("deploy frequency graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "deploy_frequency");
        assert_eq!(audit.decision, "elite-cadence");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "deploy_frequency");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("deploy frequency graph jsonl")
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
