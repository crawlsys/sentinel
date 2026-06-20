//! Graph-backed BA draft classification.
//!
//! The BA orchestrator emits the recommendation envelope consumed by BA1,
//! BA3, and A13. This graph validates the aggregate structure immediately at
//! draft time and emits a checkpointed verdict, so `sentinel ba draft` and the
//! MCP BA surface are durable LangGraph authority paths rather than plain LLM
//! output printers.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_domain::ba::BaRecommendation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BaDraftDecision {
    #[default]
    Unclassified,
    MissingBody,
    MissingCitations,
    MissingRequirements,
    IncompleteSpecChallenge,
    Ready,
    HighRiskReady,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaDraftState {
    pub identifier: String,
    pub audience: String,
    pub body_present: bool,
    pub citation_count: u64,
    pub distinct_citation_count: u64,
    pub requirement_ref_count: u64,
    pub spec_challenge_complete: bool,
    pub structurally_ready: bool,
    pub decision: BaDraftDecision,
}

impl BaDraftState {
    #[must_use]
    pub fn from_recommendation(recommendation: &BaRecommendation) -> Self {
        Self {
            identifier: recommendation.recommendation_id.as_str().to_string(),
            audience: recommendation.stakeholder_audience.key().to_string(),
            body_present: !recommendation.body.trim().is_empty(),
            citation_count: recommendation.citations.len() as u64,
            distinct_citation_count: recommendation.distinct_citation_count() as u64,
            requirement_ref_count: recommendation.requirement_refs.len() as u64,
            spec_challenge_complete: recommendation.spec_challenge.is_complete(),
            structurally_ready: recommendation.is_structurally_ready_for_publication(),
            decision: BaDraftDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BaDraftGraphRun {
    pub state: BaDraftState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<BaDraftState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct BaDraftAuthorization {
    decision: BaDraftDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl BaDraftAuthorization {
    #[must_use]
    pub fn decision(&self) -> BaDraftDecision {
        self.decision
    }

    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl BaDraftGraphRun {
    #[must_use]
    pub fn ba_draft_authorization(&self) -> Result<Option<BaDraftAuthorization>, String> {
        if self.state.decision == BaDraftDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "ba_draft",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(BaDraftAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const MISSING_BODY: &str = "missing_body";
const MISSING_CITATIONS: &str = "missing_citations";
const MISSING_REQUIREMENTS: &str = "missing_requirements";
const INCOMPLETE_SPEC_CHALLENGE: &str = "incomplete_spec_challenge";
const READY: &str = "ready";
const HIGH_RISK_READY: &str = "high_risk_ready";

pub type BaDraftGraph = CompilationResult<BaDraftState>;

#[must_use]
pub fn ba_draft_decision_label(decision: BaDraftDecision) -> &'static str {
    match decision {
        BaDraftDecision::Unclassified => "unclassified",
        BaDraftDecision::MissingBody => "missing-body",
        BaDraftDecision::MissingCitations => "missing-citations",
        BaDraftDecision::MissingRequirements => "missing-requirements",
        BaDraftDecision::IncompleteSpecChallenge => "incomplete-spec-challenge",
        BaDraftDecision::Ready => "ready",
        BaDraftDecision::HighRiskReady => "high-risk-ready",
    }
}

fn expected_decision(state: &BaDraftState) -> BaDraftDecision {
    if !state.body_present {
        BaDraftDecision::MissingBody
    } else if state.citation_count == 0 || state.distinct_citation_count == 0 {
        BaDraftDecision::MissingCitations
    } else if state.requirement_ref_count == 0 {
        BaDraftDecision::MissingRequirements
    } else if !state.spec_challenge_complete {
        BaDraftDecision::IncompleteSpecChallenge
    } else if matches!(state.audience.as_str(), "exec" | "board") {
        BaDraftDecision::HighRiskReady
    } else {
        BaDraftDecision::Ready
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "ba_draft")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn ba_draft_state_schema() -> StateSchema<BaDraftState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "audience",
                "body_present",
                "citation_count",
                "distinct_citation_count",
                "requirement_ref_count",
                "spec_challenge_complete",
                "structurally_ready",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "audience": {
                    "type": "string",
                    "enum": ["exec", "board", "customer", "internal_team"]
                },
                "body_present": { "type": "boolean" },
                "citation_count": { "type": "integer", "minimum": 0 },
                "distinct_citation_count": { "type": "integer", "minimum": 0 },
                "requirement_ref_count": { "type": "integer", "minimum": 0 },
                "spec_challenge_complete": { "type": "boolean" },
                "structurally_ready": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "MissingBody",
                        "MissingCitations",
                        "MissingRequirements",
                        "IncompleteSpecChallenge",
                        "Ready",
                        "HighRiskReady"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "ba_draft",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &BaDraftState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "ba_draft identifier must not be empty".to_string(),
                ));
            }
            if !matches!(
                state.audience.as_str(),
                "exec" | "board" | "customer" | "internal_team"
            ) {
                return Err(StateError::ValidationFailed(
                    "ba_draft audience must be a known stakeholder audience".to_string(),
                ));
            }
            if state.distinct_citation_count > state.citation_count {
                return Err(StateError::ValidationFailed(
                    "ba_draft distinct_citation_count cannot exceed citation_count".to_string(),
                ));
            }
            let expected_ready = state.body_present
                && state.citation_count > 0
                && state.requirement_ref_count > 0
                && state.spec_challenge_complete;
            if state.structurally_ready != expected_ready {
                return Err(StateError::ValidationFailed(
                    "ba_draft structurally_ready must match draft structure inputs".to_string(),
                ));
            }
            if state.decision != BaDraftDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(
                    "ba_draft terminal decision must match draft structure inputs".to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_ba_draft_graph() -> Result<BaDraftGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("ba_draft").await?;
    build_ba_draft_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_ba_draft_graph_with_ephemeral_sqlite() -> Result<BaDraftGraph, String> {
    build_ba_draft_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_ba_draft_graph_with_database_path(
    database_path: &str,
) -> Result<BaDraftGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_ba_draft_graph_with_checkpointer(checkpointer).await
}

async fn build_ba_draft_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<BaDraftGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = ba_draft_state_schema();
    let builder = StateGraphBuilder::<BaDraftState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            MISSING_BODY,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", MISSING_BODY, &s.identifier)?;
                let mut next = s;
                next.decision = BaDraftDecision::MissingBody;
                Ok::<_, NodeError>(next)
            },
            node_config(
                MISSING_BODY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            MISSING_CITATIONS,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", MISSING_CITATIONS, &s.identifier)?;
                let mut next = s;
                next.decision = BaDraftDecision::MissingCitations;
                Ok::<_, NodeError>(next)
            },
            node_config(
                MISSING_CITATIONS,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            MISSING_REQUIREMENTS,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", MISSING_REQUIREMENTS, &s.identifier)?;
                let mut next = s;
                next.decision = BaDraftDecision::MissingRequirements;
                Ok::<_, NodeError>(next)
            },
            node_config(
                MISSING_REQUIREMENTS,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            INCOMPLETE_SPEC_CHALLENGE,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", INCOMPLETE_SPEC_CHALLENGE, &s.identifier)?;
                let mut next = s;
                next.decision = BaDraftDecision::IncompleteSpecChallenge;
                Ok::<_, NodeError>(next)
            },
            node_config(
                INCOMPLETE_SPEC_CHALLENGE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            READY,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", READY, &s.identifier)?;
                let mut next = s;
                next.decision = BaDraftDecision::Ready;
                Ok::<_, NodeError>(next)
            },
            node_config(
                READY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            HIGH_RISK_READY,
            |s: BaDraftState| async move {
                emit_decision_node_event("ba_draft", HIGH_RISK_READY, &s.identifier)?;
                let mut next = s;
                next.decision = BaDraftDecision::HighRiskReady;
                Ok::<_, NodeError>(next)
            },
            node_config(
                HIGH_RISK_READY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &BaDraftState| match expected_decision(s) {
            BaDraftDecision::MissingBody => MISSING_BODY.into(),
            BaDraftDecision::MissingCitations => MISSING_CITATIONS.into(),
            BaDraftDecision::MissingRequirements => MISSING_REQUIREMENTS.into(),
            BaDraftDecision::IncompleteSpecChallenge => INCOMPLETE_SPEC_CHALLENGE.into(),
            BaDraftDecision::Ready => READY.into(),
            BaDraftDecision::HighRiskReady => HIGH_RISK_READY.into(),
            BaDraftDecision::Unclassified => MISSING_BODY.into(),
        })
        .add_edge(MISSING_BODY, END)
        .add_edge(MISSING_CITATIONS, END)
        .add_edge(MISSING_REQUIREMENTS, END)
        .add_edge(INCOMPLETE_SPEC_CHALLENGE, END)
        .add_edge(READY, END)
        .add_edge(HIGH_RISK_READY, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_ba_draft_decision_report(
    compiled: &BaDraftGraph,
    state: BaDraftState,
) -> Result<BaDraftGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "ba_draft",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "ba_draft", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "ba_draft",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(BaDraftGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: ba_draft_graph_topology(compiled)?,
    })
}

pub fn ba_draft_graph_topology(compiled: &BaDraftGraph) -> Result<DecisionGraphTopology, String> {
    topology("ba_draft", compiled)
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

    fn complete_challenge() -> SpecChallenge {
        SpecChallenge {
            work_id: WorkId::new("ba-draft-work").unwrap(),
            agent_id: "ba-orchestrator".to_string(),
            challenged_spec: SpecReference {
                hash: "brief-hash".to_string(),
                source: "stakeholder brief".to_string(),
            },
            reversibility_class: ReversibilityClass::Catastrophic,
            assumptions: ChallengeCategory::new(vec![Assumption {
                statement: "stakeholder wants growth".to_string(),
                confidence: AssumptionConfidence::Medium,
                blast_if_wrong: ReversibilityClass::Irreversible,
            }]),
            gaps: ChallengeCategory::new(vec![SpecGap {
                topic: "budget".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            }]),
            ambiguities: ChallengeCategory::new(vec![Ambiguity {
                spec_excerpt: "scale up".to_string(),
                interpretations: vec!["more users".to_string(), "more revenue".to_string()],
                chosen: "more users".to_string(),
                rationale: "brief mentions adoption".to_string(),
            }]),
            alternatives_considered: ChallengeCategory::new(vec![Alternative {
                description: "vertical scaling".to_string(),
                why_rejected: "worse unit economics".to_string(),
            }]),
            constraints_not_satisfied: ChallengeCategory::none("all constraints satisfied"),
            created_at: ts(),
        }
    }

    fn recommendation(audience: StakeholderAudience) -> BaRecommendation {
        BaRecommendation {
            recommendation_id: RecommendationId::new("rec-enterprise").unwrap(),
            brief: "scale the platform".to_string(),
            stakeholder_audience: audience,
            body: "Recommend horizontal scaling with phased rollout.".to_string(),
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
            spec_challenge: complete_challenge(),
            generated_at: ts(),
            agent_id: "ba-orchestrator".to_string(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_high_risk_ready_exec_draft() {
        let graph = build_ba_draft_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = BaDraftState::from_recommendation(&recommendation(StakeholderAudience::Exec));
        let run = run_ba_draft_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, BaDraftDecision::HighRiskReady);
        assert_eq!(run.topology.graph, "ba_draft");
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state write history"
        );
        let authorization = run
            .ba_draft_authorization()
            .expect("ba draft should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.decision(), BaDraftDecision::HighRiskReady);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_ready_internal_team_draft() {
        let graph = build_ba_draft_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state =
            BaDraftState::from_recommendation(&recommendation(StakeholderAudience::InternalTeam));
        let run = run_ba_draft_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, BaDraftDecision::Ready);
        assert_eq!(
            run.ba_draft_authorization()
                .expect("authorization")
                .expect("authorization")
                .decision(),
            BaDraftDecision::Ready
        );
    }

    #[tokio::test]
    async fn graph_prioritizes_missing_requirements() {
        let graph = build_ba_draft_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut recommendation = recommendation(StakeholderAudience::Customer);
        recommendation.requirement_refs.clear();
        let state = BaDraftState::from_recommendation(&recommendation);
        let run = run_ba_draft_decision_report(&graph, state)
            .await
            .expect("graph runs");

        assert_eq!(run.state.decision, BaDraftDecision::MissingRequirements);
    }

    #[tokio::test]
    async fn graph_schema_rejects_inconsistent_citation_counts() {
        let graph = build_ba_draft_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state =
            BaDraftState::from_recommendation(&recommendation(StakeholderAudience::Exec));
        state.distinct_citation_count = state.citation_count + 1;

        let err = run_ba_draft_decision_report(&graph, state)
            .await
            .expect_err("invalid counts must fail");
        assert!(
            err.contains("distinct_citation_count"),
            "unexpected error: {err}"
        );
    }
}
