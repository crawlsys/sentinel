use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use sentinel_application::dev_scorecard::DevScore;
use sentinel_application::mcp_handler::{DevScorecardGraphAudit, DevScorecardGraphAuditRun};
use sentinel_infrastructure::dev_scorecard_graph::{
    build_dev_scorecard_graph, dev_scorecard_decision_label, run_dev_scorecard_decision_report,
    DevScorecardDecision, DevScorecardState,
};

pub(crate) async fn run_dev_scorecard_graph_audit(
    scores: &[DevScore],
    graph_jsonl: &Path,
) -> Result<DevScorecardGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dev scorecard graph dir {}", parent.display()))?;
    }
    let graph = build_dev_scorecard_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build dev scorecard graph: {e}"))?;
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create dev scorecard graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut runs = Vec::with_capacity(scores.len());
    let mut attribution_divergences = 0usize;
    let mut excellent = 0usize;
    let mut healthy = 0usize;
    let mut needs_attention = 0usize;

    for score in scores {
        let state = DevScorecardState::from_score(score);
        let run = run_dev_scorecard_decision_report(&graph, state)
            .await
            .map_err(|e| anyhow::anyhow!("dev scorecard graph failed for {}: {e}", score.name))?;
        let authorization = run
            .scorecard_authorization()
            .map_err(|e| anyhow::anyhow!("dev scorecard graph authorization failed: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "dev scorecard graph produced no authorization for {}",
                    score.name
                )
            })?;
        let authorization_checkpoint = Some(authorization.checkpoint_ref());
        let decision = dev_scorecard_decision_label(run.state.decision).to_string();
        match run.state.decision {
            DevScorecardDecision::AttributionDivergence => attribution_divergences += 1,
            DevScorecardDecision::Excellent => excellent += 1,
            DevScorecardDecision::Healthy => healthy += 1,
            DevScorecardDecision::NeedsAttention => needs_attention += 1,
            DevScorecardDecision::Unclassified => {}
        }
        let thread_id = run.thread_id.clone();
        let run_json = serde_json::to_value(&run)?;
        let row = serde_json::json!({
            "workflow_authority": "langgraph",
            "graph": "dev_scorecard",
            "identifier": score.name.clone(),
            "decision": decision.clone(),
            "authorization_checkpoint": authorization_checkpoint.clone(),
            "thread_id": thread_id.clone(),
            "run": run_json,
        });
        serde_json::to_writer(&mut writer, &row).with_context(|| {
            format!(
                "write dev scorecard graph row for {} to {}",
                score.name,
                graph_jsonl.display()
            )
        })?;
        writer.write_all(b"\n").with_context(|| {
            format!(
                "terminate dev scorecard graph row for {} in {}",
                score.name,
                graph_jsonl.display()
            )
        })?;
        runs.push(DevScorecardGraphAuditRun {
            identifier: score.name.clone(),
            decision,
            authorization_checkpoint,
            thread_id,
            run: row["run"].clone(),
        });
    }

    writer
        .flush()
        .with_context(|| format!("flush dev scorecard graph {}", graph_jsonl.display()))?;
    Ok(DevScorecardGraphAudit {
        workflow_authority: "langgraph",
        graph: "dev_scorecard",
        graph_runs_path: graph_jsonl.to_path_buf(),
        devs_audited: scores.len(),
        attribution_divergences,
        excellent,
        healthy,
        needs_attention,
        runs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn scorecard_audit_runs_devs_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("dev-scorecard.graph-runs.jsonl");
        let scores = vec![
            DevScore {
                name: "Rene".to_string(),
                commits: 80,
                active_days: 12,
                merged_prs: 20,
                commits_per_active_day: 6.7,
                prs_per_week: 11.7,
                delivered_tickets: 8,
                clean_tickets: 7,
                bounced_tickets: 1,
                first_pass_qa_rate: 0.88,
                score: 92.0,
                attribution_divergence: true,
            },
            DevScore {
                name: "Ada".to_string(),
                commits: 50,
                active_days: 10,
                merged_prs: 10,
                commits_per_active_day: 5.0,
                prs_per_week: 7.0,
                delivered_tickets: 4,
                clean_tickets: 4,
                bounced_tickets: 0,
                first_pass_qa_rate: 1.0,
                score: 86.0,
                attribution_divergence: false,
            },
        ];

        let audit = run_dev_scorecard_graph_audit(&scores, &graph_jsonl)
            .await
            .expect("dev scorecard graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "dev_scorecard");
        assert_eq!(audit.devs_audited, 2);
        assert_eq!(audit.attribution_divergences, 1);
        assert_eq!(audit.excellent, 1);
        assert_eq!(audit.runs[0].decision, "attribution-divergence");
        assert!(audit.runs[0]
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.runs[0].run["topology"]["graph"], "dev_scorecard");
        assert!(audit.runs[0].run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("scorecard graph jsonl")
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
