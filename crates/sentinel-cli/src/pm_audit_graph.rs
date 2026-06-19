use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

use sentinel_application::linear_pm_audit::PmFlag;
use sentinel_application::mcp_handler::{PmAuditGraphAudit, PmAuditGraphAuditRun};
use sentinel_infrastructure::pm_audit_graph::{
    build_pm_audit_graph, pm_audit_decision_label, run_pm_audit_decision_report, PmAuditDecision,
    PmAuditState,
};

pub(crate) async fn run_pm_audit_graph_audit(
    flags: &[PmFlag],
    graph_jsonl: &Path,
) -> Result<PmAuditGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create PM audit graph dir {}", parent.display()))?;
    }
    let graph = build_pm_audit_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build PM audit graph: {e}"))?;
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create PM audit graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut runs = Vec::with_capacity(flags.len());
    let mut hard_violations = 0usize;
    let mut advisory_flags = 0usize;
    let mut cleared = 0usize;

    for flag in flags {
        let state = PmAuditState::from_flag(flag);
        let run = run_pm_audit_decision_report(&graph, state)
            .await
            .map_err(|e| anyhow::anyhow!("PM audit graph failed for {}: {e}", flag.identifier))?;
        let authorization_checkpoint = run
            .flag_authorization()
            .map_err(|e| {
                anyhow::anyhow!(
                    "PM audit graph authorization failed for {}: {e}",
                    flag.identifier
                )
            })?
            .map(|authorization| authorization.checkpoint_ref());
        let decision = pm_audit_decision_label(run.state.decision).to_string();
        match run.state.decision {
            PmAuditDecision::HardViolation => hard_violations += 1,
            PmAuditDecision::Advisory => advisory_flags += 1,
            PmAuditDecision::Clear => cleared += 1,
        }
        let thread_id = run.thread_id.clone();
        let run_json = serde_json::to_value(&run)?;
        let row = serde_json::json!({
            "workflow_authority": "langgraph",
            "graph": "pm_audit",
            "identifier": flag.identifier.clone(),
            "category": flag.category.clone(),
            "decision": decision.clone(),
            "authorization_checkpoint": authorization_checkpoint.clone(),
            "thread_id": thread_id.clone(),
            "run": run_json,
        });
        serde_json::to_writer(&mut writer, &row).with_context(|| {
            format!(
                "write PM audit graph row for {} to {}",
                flag.identifier,
                graph_jsonl.display()
            )
        })?;
        writer.write_all(b"\n").with_context(|| {
            format!(
                "terminate PM audit graph row for {} in {}",
                flag.identifier,
                graph_jsonl.display()
            )
        })?;
        runs.push(PmAuditGraphAuditRun {
            identifier: flag.identifier.clone(),
            category: flag.category.clone(),
            decision,
            authorization_checkpoint,
            thread_id,
            run: row["run"].clone(),
        });
    }

    writer
        .flush()
        .with_context(|| format!("flush PM audit graph {}", graph_jsonl.display()))?;
    Ok(PmAuditGraphAudit {
        workflow_authority: "langgraph",
        graph: "pm_audit",
        graph_runs_path: graph_jsonl.to_path_buf(),
        flags_audited: flags.len(),
        hard_violations,
        advisory_flags,
        cleared,
        runs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn pm_audit_runs_flags_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("pm-audit.graph-runs.jsonl");
        let flags = vec![
            PmFlag {
                identifier: "FPCRM-901".to_string(),
                title: "Oversized work".to_string(),
                category: "oversized".to_string(),
                estimate: Some(8.0),
                state: "Backlog".to_string(),
                detail: "8-pt ticket still open".to_string(),
            },
            PmFlag {
                identifier: "FPCRM-902".to_string(),
                title: "QA bounced".to_string(),
                category: "qa-failed".to_string(),
                estimate: Some(3.0),
                state: "QA Failed".to_string(),
                detail: "bounced QA".to_string(),
            },
        ];

        let audit = run_pm_audit_graph_audit(&flags, &graph_jsonl)
            .await
            .expect("PM audit graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "pm_audit");
        assert_eq!(audit.flags_audited, 2);
        assert_eq!(audit.hard_violations, 1);
        assert_eq!(audit.advisory_flags, 1);
        assert_eq!(audit.runs[0].decision, "hard-violation");
        assert!(audit.runs[0]
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.runs[0].run["topology"]["graph"], "pm_audit");
        assert!(audit.runs[0].run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("PM graph jsonl")
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
