use anyhow::{Context, Result};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use sentinel_application::mcp_handler::{SeverityGraphAudit, SeverityGraphAuditRun};
use sentinel_application::severity::SeverityProposal;
use sentinel_infrastructure::severity_graph::{
    build_severity_mutation_graph, run_severity_mutation_decision_report, SeverityMutationState,
};

pub(crate) fn load_severity_proposals(path: &Path) -> Result<Vec<SeverityProposal>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read severity proposals {}", path.display()))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            serde_json::from_str::<SeverityProposal>(line).with_context(|| {
                format!(
                    "parse severity proposal {} line {}",
                    path.display(),
                    idx + 1
                )
            })
        })
        .collect()
}

pub(crate) async fn audit_severity_proposals(
    proposals: &[SeverityProposal],
    graph_jsonl: &Path,
) -> Result<SeverityGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create severity graph audit dir {}", parent.display()))?;
    }
    let graph = build_severity_mutation_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build severity mutation graph: {e}"))?;
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create severity graph audit {}", graph_jsonl.display()))?;
    let mut writer = BufWriter::new(file);
    let mut runs = Vec::with_capacity(proposals.len());
    let mut authorized_sets = 0usize;
    let mut skipped = 0usize;

    for proposal in proposals {
        let state = SeverityMutationState::from_proposal(proposal);
        let run = run_severity_mutation_decision_report(&graph, state)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "severity graph audit failed for {}: {e}",
                    proposal.identifier
                )
            })?;
        let authorization_checkpoint = run
            .apply_authorization()
            .map_err(|e| anyhow::anyhow!("severity graph authorization failed: {e}"))?
            .map(|auth| auth.checkpoint_ref());
        let decision = format!("{:?}", run.state.decision).to_ascii_lowercase();
        if authorization_checkpoint.is_some() {
            authorized_sets += 1;
        } else {
            skipped += 1;
        }
        let thread_id = run.thread_id.clone();
        let run_json = serde_json::to_value(&run)?;
        let row = severity_graph_row(
            &proposal.identifier,
            &decision,
            authorization_checkpoint.as_deref(),
            &thread_id,
            None,
            run_json,
        );
        serde_json::to_writer(&mut writer, &row).with_context(|| {
            format!(
                "write severity graph audit row for {} to {}",
                proposal.identifier,
                graph_jsonl.display()
            )
        })?;
        writer.write_all(b"\n").with_context(|| {
            format!(
                "terminate severity graph audit row for {} in {}",
                proposal.identifier,
                graph_jsonl.display()
            )
        })?;
        runs.push(SeverityGraphAuditRun {
            identifier: proposal.identifier.clone(),
            decision,
            authorization_checkpoint,
            thread_id,
            run: row["run"].clone(),
        });
    }

    writer
        .flush()
        .with_context(|| format!("flush severity graph audit {}", graph_jsonl.display()))?;
    Ok(SeverityGraphAudit {
        workflow_authority: "langgraph",
        graph: "severity",
        graph_runs_path: PathBuf::from(graph_jsonl),
        proposals_audited: proposals.len(),
        authorized_sets,
        skipped,
        runs,
    })
}

pub(crate) fn severity_graph_row(
    identifier: &str,
    decision: &str,
    authorization_checkpoint: Option<&str>,
    thread_id: &str,
    applied: Option<bool>,
    run: serde_json::Value,
) -> serde_json::Value {
    let mut row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "severity",
        "identifier": identifier,
        "decision": decision,
        "authorization_checkpoint": authorization_checkpoint,
        "thread_id": thread_id,
        "run": run,
    });
    if let Some(applied) = applied {
        row["applied"] = serde_json::Value::Bool(applied);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[tokio::test]
    async fn audit_emits_langgraph_checkpoint_evidence() {
        let _env_guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_sentinel_home = std::env::var_os("SENTINEL_HOME");
        let prev_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        let prev_pg_url = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        let prev_pg_schema = std::env::var_os("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_URL");
        std::env::remove_var("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA");

        let proposal = SeverityProposal {
            issue_id: Some("linear-issue-id".to_string()),
            identifier: "FPCRM-777".to_string(),
            title: "Core workflow broken".to_string(),
            current_priority: Some(0),
            proposed_priority: 2,
            reasoning: "core workflow impact".to_string(),
            action: "set".to_string(),
            opus_priority: 2,
            gpt_priority: 2,
            models_agreed: true,
        };
        let graph_jsonl = tmp.path().join("severity.graph-runs.jsonl");

        let audit = audit_severity_proposals(&[proposal], &graph_jsonl)
            .await
            .expect("severity graph audit");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "severity");
        assert_eq!(audit.proposals_audited, 1);
        assert_eq!(audit.authorized_sets, 1);
        assert_eq!(audit.skipped, 0);
        assert_eq!(audit.runs[0].identifier, "FPCRM-777");
        assert_eq!(audit.runs[0].decision, "set");
        assert!(audit.runs[0]
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.runs[0].run["topology"]["graph"], "severity");
        assert_eq!(audit.runs[0].run["topology"]["durable_checkpointer"], true);
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"graph\":\"severity\""));

        restore_env("SENTINEL_HOME", prev_sentinel_home);
        restore_env("SENTINEL_DECISION_GRAPH_CHECKPOINTER", prev_backend);
        restore_env("SENTINEL_DECISION_GRAPH_POSTGRES_URL", prev_pg_url);
        restore_env("SENTINEL_DECISION_GRAPH_POSTGRES_SCHEMA", prev_pg_schema);
    }
}
