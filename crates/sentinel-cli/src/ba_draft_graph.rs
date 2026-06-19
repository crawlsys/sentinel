use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use sentinel_domain::ba::BaRecommendation;
use sentinel_infrastructure::ba_draft_graph::{
    ba_draft_decision_label, build_ba_draft_graph, run_ba_draft_decision_report, BaDraftState,
};

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BaDraftGraphAudit {
    pub workflow_authority: &'static str,
    pub graph: &'static str,
    pub graph_runs_path: PathBuf,
    pub decision: String,
    pub authorization_checkpoint: Option<String>,
    pub thread_id: String,
    pub run: serde_json::Value,
}

pub(crate) async fn run_ba_draft_graph_audit(
    recommendation: &BaRecommendation,
    graph_jsonl: &Path,
) -> Result<BaDraftGraphAudit> {
    if let Some(parent) = graph_jsonl.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create BA draft graph dir {}", parent.display()))?;
    }
    let graph = build_ba_draft_graph()
        .await
        .map_err(|e| anyhow::anyhow!("build BA draft graph: {e}"))?;
    let state = BaDraftState::from_recommendation(recommendation);
    let run = run_ba_draft_decision_report(&graph, state)
        .await
        .map_err(|e| anyhow::anyhow!("BA draft graph failed: {e}"))?;
    let authorization = run
        .ba_draft_authorization()
        .map_err(|e| anyhow::anyhow!("BA draft graph authorization failed: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("BA draft graph produced no authorization checkpoint"))?;
    let authorization_checkpoint = Some(authorization.checkpoint_ref());
    let decision = ba_draft_decision_label(run.state.decision).to_string();
    let thread_id = run.thread_id.clone();
    let run_json = serde_json::to_value(&run)?;
    let row = serde_json::json!({
        "workflow_authority": "langgraph",
        "graph": "ba_draft",
        "decision": decision.clone(),
        "authorization_checkpoint": authorization_checkpoint.clone(),
        "thread_id": thread_id.clone(),
        "run": run_json,
    });
    let file = std::fs::File::create(graph_jsonl)
        .with_context(|| format!("create BA draft graph {}", graph_jsonl.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, &row)
        .with_context(|| format!("write BA draft graph row to {}", graph_jsonl.display()))?;
    writer
        .write_all(b"\n")
        .with_context(|| format!("terminate BA draft graph row in {}", graph_jsonl.display()))?;
    writer
        .flush()
        .with_context(|| format!("flush BA draft graph {}", graph_jsonl.display()))?;
    Ok(BaDraftGraphAudit {
        workflow_authority: "langgraph",
        graph: "ba_draft",
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
    use chrono::{TimeZone, Utc};
    use sentinel_domain::ba::{
        ArtifactReference, ProvenanceClass, RecommendationId, RequirementRef, StakeholderAudience,
    };
    use sentinel_domain::reversibility::ReversibilityClass;
    use sentinel_domain::spec_challenge::{
        Alternative, Ambiguity, Assumption, AssumptionConfidence, ChallengeCategory, GapResolution,
        SpecChallenge, SpecGap, SpecReference, WorkId,
    };

    fn ts() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn recommendation() -> BaRecommendation {
        BaRecommendation {
            recommendation_id: RecommendationId::new("ba-cli-graph").unwrap(),
            brief: "scale the platform".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            body: "Recommend horizontal scaling.".to_string(),
            citations: vec![ArtifactReference {
                artifact_id: "linear://issue/FPCRM-42".to_string(),
                content_hash: "hash-1".to_string(),
                provenance_class: ProvenanceClass::SystemOfRecord,
                retrieved_at: ts(),
            }],
            requirement_refs: vec![RequirementRef {
                orchestration_id: "orch-1".to_string(),
                matrix_row_id: "row-1".to_string(),
                content_hash: "req-hash".to_string(),
                statement: "stakeholder wants growth".to_string(),
            }],
            spec_challenge: SpecChallenge {
                work_id: WorkId::new("work-1").unwrap(),
                agent_id: "ba-orchestrator".to_string(),
                challenged_spec: SpecReference {
                    hash: "brief-hash".to_string(),
                    source: "brief".to_string(),
                },
                reversibility_class: ReversibilityClass::Catastrophic,
                assumptions: ChallengeCategory::new(vec![Assumption {
                    statement: "growth matters".to_string(),
                    confidence: AssumptionConfidence::Medium,
                    blast_if_wrong: ReversibilityClass::Irreversible,
                }]),
                gaps: ChallengeCategory::new(vec![SpecGap {
                    topic: "budget".to_string(),
                    how_resolved: GapResolution::OperatorClarified,
                    inference_source: None,
                }]),
                ambiguities: ChallengeCategory::new(vec![Ambiguity {
                    spec_excerpt: "scale".to_string(),
                    interpretations: vec!["users".to_string(), "revenue".to_string()],
                    chosen: "users".to_string(),
                    rationale: "brief context".to_string(),
                }]),
                alternatives_considered: ChallengeCategory::new(vec![Alternative {
                    description: "vertical scaling".to_string(),
                    why_rejected: "cost".to_string(),
                }]),
                constraints_not_satisfied: ChallengeCategory::none("all met"),
                created_at: ts(),
            },
            generated_at: ts(),
            agent_id: "ba-orchestrator".to_string(),
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ba_draft_audit_runs_recommendation_through_langgraph() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let previous_home = std::env::var_os("SENTINEL_HOME");
        let previous_backend = std::env::var_os("SENTINEL_DECISION_GRAPH_CHECKPOINTER");
        std::env::set_var("SENTINEL_HOME", tmp.path());
        std::env::set_var("SENTINEL_DECISION_GRAPH_CHECKPOINTER", "sqlite");
        let graph_jsonl = tmp.path().join("ba-draft.graph-runs.jsonl");

        let audit = run_ba_draft_graph_audit(&recommendation(), &graph_jsonl)
            .await
            .expect("BA draft graph");

        assert_eq!(audit.workflow_authority, "langgraph");
        assert_eq!(audit.graph, "ba_draft");
        assert_eq!(audit.decision, "high-risk-ready");
        assert!(audit
            .authorization_checkpoint
            .as_deref()
            .is_some_and(|checkpoint| checkpoint.contains('#')));
        assert_eq!(audit.run["topology"]["graph"], "ba_draft");
        assert!(audit.run["checkpoints"]
            .as_array()
            .is_some_and(|entries| !entries.is_empty()));
        assert!(std::fs::read_to_string(&graph_jsonl)
            .expect("BA draft graph jsonl")
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
