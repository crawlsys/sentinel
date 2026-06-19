use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use sentinel_application::linear_code_audit::CodeFlag;
use sentinel_application::mcp_handler::{
    CodeReconciliationGraphAudit, CodeReconciliationGraphAuditRun,
};
use sentinel_infrastructure::decision_graph_introspection::terminal_decision_checkpoint_result;
use sentinel_infrastructure::reconciliation_graph::{
    build_reconciliation_graph, run_recon_decision_report, ReconState, ReconVerdict,
};

pub(crate) async fn run_code_reconciliation_graph_audit(
    flags: &[CodeFlag],
    graph_jsonl: &Path,
) -> Result<CodeReconciliationGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create reconciliation graph audit dir {}", parent.display())
        })?;
    }
    let graph = build_reconciliation_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build reconciliation graph: {e}"))?;
    let file = std::fs::File::create(graph_jsonl).with_context(|| {
        format!(
            "create reconciliation graph audit {}",
            graph_jsonl.display()
        )
    })?;
    let mut writer = std::io::BufWriter::new(file);
    let mut runs = Vec::with_capacity(flags.len());
    let mut authorized_flags = 0usize;
    let mut cleared = 0usize;

    for flag in flags {
        let mut state = ReconState::new(
            flag.identifier.clone(),
            flag.detail.clone(),
            format!(
                "linear-code-audit category={} commits={} files={} state={}",
                flag.category, flag.commits, flag.files, flag.state
            ),
        );
        state.verdict = Some(ReconVerdict::Reverted);
        let run = run_recon_decision_report(&graph, state)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "reconciliation graph audit failed for {}: {e}",
                    flag.identifier
                )
            })?;
        let authorization_checkpoint = run
            .flag_authorization()
            .map_err(|e| {
                anyhow::anyhow!(
                    "reconciliation graph authorization failed for {}: {e}",
                    flag.identifier
                )
            })?
            .map(|authorization| authorization.checkpoint_ref());
        let terminal_checkpoint = terminal_decision_checkpoint_result(
            "reconciliation",
            &run.thread_id,
            &run.state,
            &run.checkpoints,
            &run.write_history,
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "reconciliation graph terminal checkpoint failed for {}: {e}",
                flag.identifier
            )
        })?
        .checkpoint_ref();
        let decision = format!("{:?}", run.state.decision).to_ascii_lowercase();
        if authorization_checkpoint.is_some() {
            authorized_flags += 1;
        } else {
            cleared += 1;
        }
        let thread_id = run.thread_id.clone();
        let run_json = serde_json::to_value(&run)?;
        let row = serde_json::json!({
            "workflow_authority": "langgraph",
            "graph": "reconciliation",
            "identifier": flag.identifier,
            "decision": decision.clone(),
            "terminal_checkpoint": terminal_checkpoint.clone(),
            "authorization_checkpoint": authorization_checkpoint.clone(),
            "thread_id": thread_id.clone(),
            "run": run_json,
        });
        serde_json::to_writer(&mut writer, &row).with_context(|| {
            format!(
                "write reconciliation graph audit row for {} to {}",
                flag.identifier,
                graph_jsonl.display()
            )
        })?;
        writer.write_all(b"\n").with_context(|| {
            format!(
                "terminate reconciliation graph audit row for {} in {}",
                flag.identifier,
                graph_jsonl.display()
            )
        })?;
        runs.push(CodeReconciliationGraphAuditRun {
            identifier: flag.identifier.clone(),
            decision,
            terminal_checkpoint,
            authorization_checkpoint,
            thread_id,
            run: row["run"].clone(),
        });
    }

    writer
        .flush()
        .with_context(|| format!("flush reconciliation graph audit {}", graph_jsonl.display()))?;
    Ok(CodeReconciliationGraphAudit {
        workflow_authority: "langgraph",
        graph: "reconciliation",
        graph_runs_path: graph_jsonl.to_path_buf(),
        flags_audited: flags.len(),
        authorized_flags,
        cleared,
        runs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn reconciliation_audit_runs_flags_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("reconciliation.graph-runs.jsonl");
        let flags = vec![CodeFlag {
            identifier: "FPCRM-888".to_string(),
            state: "Completed".to_string(),
            category: "done-no-evidence".to_string(),
            commits: 0,
            files: 0,
            detail: "marked Completed but no commits or touched files found".to_string(),
        }];

        let audit = run_code_reconciliation_graph_audit(&flags, &graph_jsonl)
            .await
            .expect("reconciliation audit");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "reconciliation");
        assert_eq!(audit.flags_audited, 1);
        assert_eq!(audit.authorized_flags, 1);
        assert_eq!(audit.runs[0].identifier, "FPCRM-888");
        assert_eq!(audit.runs[0].decision, "flag");
        assert!(audit.runs[0].terminal_checkpoint.contains('#'));
        assert!(audit.runs[0]
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.runs[0].run["topology"]["graph"], "reconciliation");
        assert!(audit.runs[0].run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        let graph_rows = std::fs::read_to_string(&graph_jsonl).expect("graph jsonl");
        assert!(graph_rows.contains("\"workflow_authority\":\"langgraph\""));
        assert!(graph_rows.contains("\"terminal_checkpoint\""));

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
