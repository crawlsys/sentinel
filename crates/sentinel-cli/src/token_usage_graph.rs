use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::tokens::ScanReport;
use sentinel_infrastructure::token_usage_graph::{
    build_token_usage_graph, run_token_usage_decision_report, token_usage_decision_label,
    TokenUsageState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TokenUsageGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_token_usage_graph_audit(
    report: &ScanReport,
    graph_jsonl: &Path,
) -> Result<TokenUsageGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create token usage graph dir {}", parent.display()))?;
    }
    let graph = build_token_usage_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build token usage graph: {e}"))?;
    let state = TokenUsageState::from_report(report);
    let run = run_token_usage_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("token usage graph failed: {e}"))?;
    let authorization = run
        .token_usage_authorization()
        .map_err(|e| anyhow::anyhow!("token usage graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("token usage graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = token_usage_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "token_usage",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create token usage graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write token usage graph row to {}", graph_jsonl.display()))?;
    writer.write_all(b"\n").with_context(|| {
        format!(
            "terminate token usage graph row in {}",
            graph_jsonl.display()
        )
    })?;
    writer
        .flush()
        .with_context(|| format!("flush token usage graph {}", graph_jsonl.display()))?;
    Ok(TokenUsageGraphAudit {
        workflow_authority: "langgraph",
        graph: "token_usage",
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

    fn report() -> ScanReport {
        ScanReport {
            total_sessions: 10,
            mapped_sessions: 9,
            unmapped_sessions: 1,
            unpriced_sessions: 0,
            unpriced_tokens: 0,
            tickets: 2,
            top_n_expensive: vec![("SEN-1".to_string(), 42.0), ("SEN-2".to_string(), 10.0)],
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn token_usage_audit_runs_report_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("tokens-per-ticket.graph-runs.jsonl");

        let audit = run_token_usage_graph_audit(&report(), &graph_jsonl)
            .await
            .expect("token usage graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "token_usage");
        assert_eq!(audit.decision, "healthy-usage");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "token_usage");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("token usage graph jsonl")
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
