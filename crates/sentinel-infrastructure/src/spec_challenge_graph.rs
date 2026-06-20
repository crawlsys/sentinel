//! Graph-backed A13 spec-challenge authorization.
//!
//! The application hook computes the A13 facts: challenge presence,
//! deterministic completeness, persistence status, and semantic scorer
//! result. This graph authorizes the resulting allow/block decision through
//! durable LangGraph checkpoints so the PreToolUse path has no uncheckpointed
//! production branch for high-stakes work.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::spec_challenge_gate::{
    A13EnforcementMode, SpecChallengeEvaluation,
};
use sentinel_domain::reversibility::ReversibilityClass;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SpecChallengeDecision {
    #[default]
    Unclassified,
    Allow,
    ObserveOnlyWouldBlock,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecChallengeState {
    pub identifier: String,
    pub mode: String,
    pub class: String,
    pub challenge_present: bool,
    pub missing_required_challenge: bool,
    pub malformed_challenge: bool,
    pub completeness_finding_count: u64,
    pub store_error_count: u64,
    pub scoring_required: bool,
    pub scorer_missing: bool,
    pub scorer_error_count: u64,
    pub scorer_rejected: bool,
    pub score_min_axis_millis: u64,
    pub threshold_millis: u64,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: SpecChallengeDecision,
}

impl SpecChallengeState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &SpecChallengeEvaluation,
    ) -> Self {
        let missing_required_challenge = evaluation.missing_required_challenge;
        let malformed_challenge = evaluation.malformed_challenge;
        let store_error_count = u64::from(evaluation.store_error.is_some());
        let scorer_missing = evaluation.scorer_missing;
        let scorer_error_count = u64::from(evaluation.scorer_error.is_some());
        let scorer_rejected = evaluation.scorer_rejected;
        let completeness_finding_count = evaluation.completeness_finding_count as u64;
        let blocking_finding_count = u64::from(missing_required_challenge)
            + u64::from(malformed_challenge)
            + completeness_finding_count
            + store_error_count
            + u64::from(scorer_missing)
            + scorer_error_count
            + u64::from(scorer_rejected);
        let mode = enforcement_mode_label(evaluation.mode).to_string();
        Self {
            identifier: identifier.into(),
            mode: mode.clone(),
            class: reversibility_class_label(evaluation.class).to_string(),
            challenge_present: evaluation.challenge.is_some(),
            missing_required_challenge,
            malformed_challenge,
            completeness_finding_count,
            store_error_count,
            scoring_required: evaluation.scoring_required,
            scorer_missing,
            scorer_error_count,
            scorer_rejected,
            score_min_axis_millis: evaluation
                .score
                .map(|score| score.min_axis())
                .map(axis_to_millis)
                .unwrap_or(0),
            threshold_millis: axis_to_millis(evaluation.catastrophic_axis_threshold),
            blocking_finding_count,
            should_block: expected_should_block(&mode, blocking_finding_count),
            decision: SpecChallengeDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecChallengeRun {
    pub state: SpecChallengeState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SpecChallengeState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct SpecChallengeAuthorization {
    decision: SpecChallengeDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl SpecChallengeAuthorization {
    #[must_use]
    pub fn decision(&self) -> SpecChallengeDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl SpecChallengeRun {
    #[must_use]
    pub fn spec_challenge_authorization(
        &self,
    ) -> Result<Option<SpecChallengeAuthorization>, String> {
        if self.state.decision == SpecChallengeDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "spec_challenge",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SpecChallengeAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const OBSERVE_ONLY_WOULD_BLOCK: &str = "observe_only_would_block";
const BLOCK: &str = "block";

pub type SpecChallengeGraph = CompilationResult<SpecChallengeState>;

#[must_use]
pub const fn enforcement_mode_label(mode: A13EnforcementMode) -> &'static str {
    match mode {
        A13EnforcementMode::ObserveOnly => "observe_only",
        A13EnforcementMode::DefaultBlocking => "default_blocking",
        A13EnforcementMode::StrictBlocking => "strict_blocking",
    }
}

#[must_use]
pub const fn reversibility_class_label(class: ReversibilityClass) -> &'static str {
    match class {
        ReversibilityClass::TriviallyReversible => "trivially_reversible",
        ReversibilityClass::ReversibleWithEffort => "reversible_with_effort",
        ReversibilityClass::Irreversible => "irreversible",
        ReversibilityClass::Catastrophic => "catastrophic",
    }
}

#[must_use]
pub fn spec_challenge_decision_label(decision: SpecChallengeDecision) -> &'static str {
    match decision {
        SpecChallengeDecision::Unclassified => "unclassified",
        SpecChallengeDecision::Allow => "allow",
        SpecChallengeDecision::ObserveOnlyWouldBlock => "observe-only-would-block",
        SpecChallengeDecision::Block => "block",
    }
}

fn axis_to_millis(axis: f32) -> u64 {
    (axis.clamp(0.0, 1.0) * 1000.0).round() as u64
}

fn expected_should_block(mode: &str, blocking_finding_count: u64) -> bool {
    match mode {
        "observe_only" => false,
        "default_blocking" | "strict_blocking" => blocking_finding_count > 0,
        _ => false,
    }
}

fn expected_decision(state: &SpecChallengeState) -> SpecChallengeDecision {
    if state.should_block {
        SpecChallengeDecision::Block
    } else if state.mode == "observe_only" && state.blocking_finding_count > 0 {
        SpecChallengeDecision::ObserveOnlyWouldBlock
    } else {
        SpecChallengeDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "spec_challenge")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn spec_challenge_state_schema() -> StateSchema<SpecChallengeState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "mode",
                "class",
                "challenge_present",
                "missing_required_challenge",
                "malformed_challenge",
                "completeness_finding_count",
                "store_error_count",
                "scoring_required",
                "scorer_missing",
                "scorer_error_count",
                "scorer_rejected",
                "score_min_axis_millis",
                "threshold_millis",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "mode": {
                    "type": "string",
                    "enum": ["observe_only", "default_blocking", "strict_blocking"]
                },
                "class": {
                    "type": "string",
                    "enum": [
                        "trivially_reversible",
                        "reversible_with_effort",
                        "irreversible",
                        "catastrophic"
                    ]
                },
                "challenge_present": { "type": "boolean" },
                "missing_required_challenge": { "type": "boolean" },
                "malformed_challenge": { "type": "boolean" },
                "completeness_finding_count": { "type": "integer", "minimum": 0 },
                "store_error_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "scoring_required": { "type": "boolean" },
                "scorer_missing": { "type": "boolean" },
                "scorer_error_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "scorer_rejected": { "type": "boolean" },
                "score_min_axis_millis": { "type": "integer", "minimum": 0, "maximum": 1000 },
                "threshold_millis": { "type": "integer", "minimum": 0, "maximum": 1000 },
                "blocking_finding_count": { "type": "integer", "minimum": 0 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "ObserveOnlyWouldBlock", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "spec_challenge",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SpecChallengeState| {
            let expected_finding_count = u64::from(state.missing_required_challenge)
                + u64::from(state.malformed_challenge)
                + state.completeness_finding_count
                + state.store_error_count
                + u64::from(state.scorer_missing)
                + state.scorer_error_count
                + u64::from(state.scorer_rejected);
            if state.blocking_finding_count != expected_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "spec_challenge blocking_finding_count must equal derived finding count: \
                     expected {expected_finding_count} from A13 finding fields, got {}",
                    state.blocking_finding_count
                )));
            }
            let expected_should_block =
                expected_should_block(&state.mode, state.blocking_finding_count);
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "spec_challenge should_block must match mode and finding counts: expected \
                     {expected_should_block} for mode={} findings={}, got {}",
                    state.mode, state.blocking_finding_count, state.should_block
                )));
            }
            if state.missing_required_challenge && state.challenge_present {
                return Err(StateError::ValidationFailed(
                    "spec_challenge missing_required_challenge cannot also have \
                     challenge_present=true"
                        .to_string(),
                ));
            }
            if state.malformed_challenge && state.challenge_present {
                return Err(StateError::ValidationFailed(
                    "spec_challenge malformed_challenge cannot also have challenge_present=true"
                        .to_string(),
                ));
            }
            if state.scorer_rejected && state.score_min_axis_millis >= state.threshold_millis {
                return Err(StateError::ValidationFailed(
                    "spec_challenge scorer_rejected requires score_min_axis_millis below threshold"
                        .to_string(),
                ));
            }
            if state.decision != SpecChallengeDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "spec_challenge terminal decision must match mode and finding counts: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: SpecChallengeState) -> Result<SpecChallengeState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_spec_challenge_graph() -> Result<SpecChallengeGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("spec_challenge").await?;
    build_spec_challenge_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_spec_challenge_graph_with_ephemeral_sqlite() -> Result<SpecChallengeGraph, String> {
    build_spec_challenge_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_spec_challenge_graph_with_database_path(
    db_path: &str,
) -> Result<SpecChallengeGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_spec_challenge_graph_with_checkpointer(checkpointer).await
}

async fn build_spec_challenge_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SpecChallengeGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = spec_challenge_state_schema();
    let builder = StateGraphBuilder::<SpecChallengeState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: SpecChallengeState| async move {
                emit_decision_node_event("spec_challenge", CLASSIFY, &s.identifier)?;
                classify_node(s).await
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
            ALLOW,
            |s: SpecChallengeState| async move {
                emit_decision_node_event("spec_challenge", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = SpecChallengeDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            OBSERVE_ONLY_WOULD_BLOCK,
            |s: SpecChallengeState| async move {
                emit_decision_node_event(
                    "spec_challenge",
                    OBSERVE_ONLY_WOULD_BLOCK,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = SpecChallengeDecision::ObserveOnlyWouldBlock;
                Ok::<_, NodeError>(next)
            },
            node_config(
                OBSERVE_ONLY_WOULD_BLOCK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK,
            |s: SpecChallengeState| async move {
                emit_decision_node_event("spec_challenge", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = SpecChallengeDecision::Block;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &SpecChallengeState| {
            match expected_decision(s) {
                SpecChallengeDecision::Allow => ALLOW.into(),
                SpecChallengeDecision::ObserveOnlyWouldBlock => OBSERVE_ONLY_WOULD_BLOCK.into(),
                SpecChallengeDecision::Block => BLOCK.into(),
                SpecChallengeDecision::Unclassified => ALLOW.into(),
            }
        })
        .add_edge(ALLOW, END)
        .add_edge(OBSERVE_ONLY_WOULD_BLOCK, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_spec_challenge_decision_report(
    compiled: &SpecChallengeGraph,
    state: SpecChallengeState,
) -> Result<SpecChallengeRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "spec_challenge",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "spec_challenge", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "spec_challenge",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SpecChallengeRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: spec_challenge_graph_topology(compiled)?,
    })
}

pub fn spec_challenge_graph_topology(
    compiled: &SpecChallengeGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("spec_challenge", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::spec_challenge_gate::SpecChallengeEvaluation;
    use sentinel_domain::ports::SpecChallengeScore;

    fn evaluation(
        mode: A13EnforcementMode,
        class: ReversibilityClass,
        would_block: bool,
    ) -> SpecChallengeEvaluation {
        SpecChallengeEvaluation {
            class,
            mode,
            catastrophic_axis_threshold: 0.7,
            challenge: None,
            malformed_challenge: false,
            missing_required_challenge: would_block,
            completeness_finding_count: 0,
            store_error: None,
            scoring_required: matches!(class, ReversibilityClass::Catastrophic),
            scorer_missing: false,
            scorer_error: None,
            score: None,
            scorer_rejected: false,
            would_block,
            should_block: mode.allows_blocking() && would_block,
            block_reason: None,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_default_blocking_missing_challenge() {
        let graph = build_spec_challenge_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SpecChallengeState::from_evaluation(
            "a13-case",
            &evaluation(
                A13EnforcementMode::DefaultBlocking,
                ReversibilityClass::Irreversible,
                true,
            ),
        );
        let run = run_spec_challenge_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SpecChallengeDecision::Block);
        assert!(run
            .spec_challenge_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_observe_only_would_block() {
        let graph = build_spec_challenge_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SpecChallengeState::from_evaluation(
            "a13-observe",
            &evaluation(
                A13EnforcementMode::ObserveOnly,
                ReversibilityClass::Catastrophic,
                true,
            ),
        );
        let run = run_spec_challenge_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            SpecChallengeDecision::ObserveOnlyWouldBlock
        );
        assert!(!run.state.should_block);
    }

    #[tokio::test]
    async fn graph_authorizes_scorer_rejection() {
        let graph = build_spec_challenge_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(
            A13EnforcementMode::DefaultBlocking,
            ReversibilityClass::Catastrophic,
            false,
        );
        eval.challenge = None;
        eval.missing_required_challenge = false;
        eval.scorer_rejected = true;
        eval.score = Some(SpecChallengeScore::new(0.9, 0.4, 0.9, 0.9, 0.9));
        eval.would_block = true;
        eval.should_block = true;
        let state = SpecChallengeState::from_evaluation("a13-score", &eval);
        let run = run_spec_challenge_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, SpecChallengeDecision::Block);
        assert_eq!(run.state.score_min_axis_millis, 400);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow() {
        let graph = build_spec_challenge_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = SpecChallengeState::from_evaluation(
            "a13-forged",
            &evaluation(
                A13EnforcementMode::DefaultBlocking,
                ReversibilityClass::Irreversible,
                true,
            ),
        );
        state.should_block = false;
        let err = run_spec_challenge_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }
}
