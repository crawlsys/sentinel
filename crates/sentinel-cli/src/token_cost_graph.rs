use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use sentinel_application::mcp_handler::TokenCostGraphAudit;
use sentinel_application::token_cost::TokenCostSummary;
use sentinel_infrastructure::token_cost_graph::{
    build_token_cost_graph, run_token_cost_decision_report, token_cost_decision_label,
    TokenCostState,
};

pub(crate) async fn run_token_cost_graph_audit(
    summary: &TokenCostSummary,
    graph_jsonl: &Path,
) -> Result<TokenCostGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create token cost graph dir {}", parent.display()))?;
    }
    let graph = build_token_cost_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build token cost graph: {e}"))?;
    let state = TokenCostState::from_summary(summary);
    let run = run_token_cost_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("token cost graph failed: {e}"))?;
    let authorization = run
        .token_cost_authorization()
        .map_err(|e| anyhow::anyhow!("token cost graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("token cost graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = token_cost_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "token_cost",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create token cost graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write token cost graph row to {}", graph_jsonl.display()))?;
    writer.write_all(b"\n").with_context(|| {
        format!(
            "terminate token cost graph row in {}",
            graph_jsonl.display()
        )
    })?;
    writer
        .flush()
        .with_context(|| format!("flush token cost graph {}", graph_jsonl.display()))?;
    Ok(TokenCostGraphAudit {
        workflow_authority: "langgraph",
        graph: "token_cost",
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
    use sentinel_application::token_cost::ModelCost;
    use std::collections::BTreeMap;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn token_cost_audit_runs_summary_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("token-cost.graph-runs.jsonl");
        let mut by_model = BTreeMap::new();
        by_model.insert(
            "opus".to_string(),
            ModelCost {
                tokens: 1_700_000,
                cached_usd: 4.25,
                uncached_usd: 8.0,
            },
        );
        let summary = TokenCostSummary {
            tickets: 2,
            total_tokens: 1_700_000,
            input_tokens: 500_000,
            output_tokens: 100_000,
            cache_write_tokens: 100_000,
            cache_read_tokens: 1_000_000,
            cost_with_caching_usd: 4.25,
            cost_without_caching_usd: 8.0,
            cache_savings_usd: 3.75,
            cache_savings_fraction: 3.75 / 8.0,
            unknown_model_tokens: 0,
            by_model,
        };

        let audit = run_token_cost_graph_audit(&summary, &graph_jsonl)
            .await
            .expect("token cost graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "token_cost");
        assert_eq!(audit.decision, "cache-effective");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "token_cost");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("token cost graph jsonl")
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
