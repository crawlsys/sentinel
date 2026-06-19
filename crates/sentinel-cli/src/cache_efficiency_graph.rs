use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_application::cache_efficiency::CacheReport;
use sentinel_infrastructure::cache_efficiency_graph::{
    build_cache_efficiency_graph, cache_efficiency_decision_label,
    run_cache_efficiency_decision_report, CacheEfficiencyState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CacheEfficiencyGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_cache_efficiency_graph_audit(
    report: &CacheReport,
    graph_jsonl: &Path,
) -> Result<CacheEfficiencyGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create cache efficiency graph dir {}", parent.display()))?;
    }
    let graph = build_cache_efficiency_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build cache efficiency graph: {e}"))?;
    let state = CacheEfficiencyState::from_report(report);
    let run = run_cache_efficiency_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("cache efficiency graph failed: {e}"))?;
    let authorization = run
        .cache_efficiency_authorization()
        .map_err(|e| anyhow::anyhow!("cache efficiency graph authorization failed: {e}"))?
        .ok_or_else(|| {
            anyhow::anyhow!("cache efficiency graph produced no authorization checkpoint")
        })?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = cache_efficiency_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "cache_efficiency",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create cache efficiency graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row).with_context(|| {
        format!(
            "write cache efficiency graph row to {}",
            graph_jsonl.display()
        )
    })?;
    writer.write_all(b"\n").with_context(|| {
        format!(
            "terminate cache efficiency graph row in {}",
            graph_jsonl.display()
        )
    })?;
    writer
        .flush()
        .with_context(|| format!("flush cache efficiency graph {}", graph_jsonl.display()))?;
    Ok(CacheEfficiencyGraphAudit {
        workflow_authority: "langgraph",
        graph: "cache_efficiency",
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
    use sentinel_application::cache_efficiency::{DailyPoint, WorstSession};

    fn report() -> CacheReport {
        CacheReport {
            sessions_scanned: 4,
            sessions_with_usage: 4,
            p50_hit_rate: 0.92,
            p90_hit_rate: 0.98,
            worst_sessions: vec![WorstSession {
                session_id: "session-1".to_string(),
                project: "sentinel".to_string(),
                date: "2026-06-18".to_string(),
                hit_rate: 0.88,
                total_input_tokens: 100_000,
                waste_estimate_usd: 1.50,
            }],
            daily_trend: vec![DailyPoint {
                date: "2026-06-18".to_string(),
                sessions: 4,
                hit_rate: 0.92,
            }],
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn cache_efficiency_audit_runs_report_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("cache-efficiency-summary.graph-runs.jsonl");

        let audit = run_cache_efficiency_graph_audit(&report(), &graph_jsonl)
            .await
            .expect("cache efficiency graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "cache_efficiency");
        assert_eq!(audit.decision, "cache-excellent");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "cache_efficiency");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("cache efficiency graph jsonl")
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
