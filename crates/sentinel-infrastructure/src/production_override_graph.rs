//! Graph-backed production override authorization.
//!
//! The application hook computes deterministic facts for the operator's
//! session-wide production override phrase: prompt presence, arm/lock signals,
//! prior armed state, lock precedence, and the target transition. This graph
//! authorizes the transition through durable LangGraph checkpoints before the
//! CLI mutates `SessionState.production_override`.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::production_override::{
    ProductionOverrideEvaluation, ProductionOverrideTransition,
};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProductionOverrideDecision {
    #[default]
    Unclassified,
    AllowNoop,
    Arm,
    Lock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionOverrideState {
    pub identifier: String,
    pub session_id_present: bool,
    pub prompt_present: bool,
    pub prompt_sha256: Option<String>,
    pub prior_armed: bool,
    pub arm_signal: bool,
    pub lock_signal: bool,
    pub lock_precedence: bool,
    pub note_present: bool,
    pub transition: String,
    pub target_armed: bool,
    pub notice_required: bool,
    pub decision: ProductionOverrideDecision,
}

impl ProductionOverrideState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &ProductionOverrideEvaluation,
    ) -> Self {
        Self {
            identifier: identifier.into(),
            session_id_present: evaluation
                .session_id
                .as_deref()
                .is_some_and(|session_id| !session_id.is_empty()),
            prompt_present: evaluation.prompt_present,
            prompt_sha256: evaluation.prompt_sha256.clone(),
            prior_armed: evaluation.prior_armed,
            arm_signal: evaluation.arm_signal,
            lock_signal: evaluation.lock_signal,
            lock_precedence: evaluation.lock_precedence,
            note_present: evaluation.note.is_some(),
            transition: transition_label(evaluation.transition).to_string(),
            target_armed: evaluation.target_armed,
            notice_required: evaluation.notice_required,
            decision: ProductionOverrideDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductionOverrideGraphRun {
    pub state: ProductionOverrideState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<ProductionOverrideState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct ProductionOverrideAuthorization {
    decision: ProductionOverrideDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl ProductionOverrideAuthorization {
    #[must_use]
    pub fn decision(&self) -> ProductionOverrideDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl ProductionOverrideGraphRun {
    #[must_use]
    pub fn production_override_authorization(
        &self,
    ) -> Result<Option<ProductionOverrideAuthorization>, String> {
        if self.state.decision == ProductionOverrideDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "production_override",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(ProductionOverrideAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW_NOOP: &str = "allow_noop";
const ARM: &str = "arm";
const LOCK: &str = "lock";

pub type ProductionOverrideGraph = CompilationResult<ProductionOverrideState>;

#[must_use]
pub const fn transition_label(transition: ProductionOverrideTransition) -> &'static str {
    match transition {
        ProductionOverrideTransition::Noop => "noop",
        ProductionOverrideTransition::Arm => "arm",
        ProductionOverrideTransition::Lock => "lock",
    }
}

#[must_use]
pub fn production_override_decision_label(decision: ProductionOverrideDecision) -> &'static str {
    match decision {
        ProductionOverrideDecision::Unclassified => "unclassified",
        ProductionOverrideDecision::AllowNoop => "allow-noop",
        ProductionOverrideDecision::Arm => "arm",
        ProductionOverrideDecision::Lock => "lock",
    }
}

fn expected_transition(state: &ProductionOverrideState) -> &'static str {
    if state.lock_signal && state.prior_armed {
        "lock"
    } else if !state.lock_signal && state.arm_signal && !state.prior_armed {
        "arm"
    } else {
        "noop"
    }
}

fn expected_decision(state: &ProductionOverrideState) -> ProductionOverrideDecision {
    match state.transition.as_str() {
        "arm" => ProductionOverrideDecision::Arm,
        "lock" => ProductionOverrideDecision::Lock,
        _ => ProductionOverrideDecision::AllowNoop,
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "production_override")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn production_override_state_schema() -> StateSchema<ProductionOverrideState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "session_id_present",
                "prompt_present",
                "prompt_sha256",
                "prior_armed",
                "arm_signal",
                "lock_signal",
                "lock_precedence",
                "note_present",
                "transition",
                "target_armed",
                "notice_required",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "session_id_present": { "type": "boolean" },
                "prompt_present": { "type": "boolean" },
                "prompt_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "prior_armed": { "type": "boolean" },
                "arm_signal": { "type": "boolean" },
                "lock_signal": { "type": "boolean" },
                "lock_precedence": { "type": "boolean" },
                "note_present": { "type": "boolean" },
                "transition": { "type": "string", "enum": ["noop", "arm", "lock"] },
                "target_armed": { "type": "boolean" },
                "notice_required": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "AllowNoop", "Arm", "Lock"]
                }
            },
            "x-sentinel": {
                "graph": "production_override",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &ProductionOverrideState| {
            if !state.prompt_present {
                if state.prompt_sha256.is_some()
                    || state.arm_signal
                    || state.lock_signal
                    || state.lock_precedence
                    || state.note_present
                    || state.transition != "noop"
                    || state.target_armed != state.prior_armed
                    || state.notice_required
                {
                    return Err(StateError::ValidationFailed(
                        "production_override missing-prompt state cannot carry transition facts"
                            .to_string(),
                    ));
                }
            } else if !optional_hex_digest_present(&state.prompt_sha256) {
                return Err(StateError::ValidationFailed(
                    "production_override prompt_sha256 must be a 64-character hex digest"
                        .to_string(),
                ));
            }
            let expected_lock_precedence = state.lock_signal && state.arm_signal;
            if state.lock_precedence != expected_lock_precedence {
                return Err(StateError::ValidationFailed(format!(
                    "production_override lock_precedence must match arm/lock signals: expected \
                     {expected_lock_precedence}, got {}",
                    state.lock_precedence
                )));
            }
            let expected_transition = expected_transition(state);
            if state.transition != expected_transition {
                return Err(StateError::ValidationFailed(format!(
                    "production_override transition must match lock precedence and prior state: \
                     expected {expected_transition}, got {}",
                    state.transition
                )));
            }
            let expected_target_armed = match expected_transition {
                "arm" => true,
                "lock" => false,
                _ => state.prior_armed,
            };
            if state.target_armed != expected_target_armed {
                return Err(StateError::ValidationFailed(format!(
                    "production_override target_armed must match transition: expected \
                     {expected_target_armed}, got {}",
                    state.target_armed
                )));
            }
            let expected_notice_required = expected_transition != "noop";
            if state.notice_required != expected_notice_required {
                return Err(StateError::ValidationFailed(format!(
                    "production_override notice_required must match transition: expected \
                     {expected_notice_required}, got {}",
                    state.notice_required
                )));
            }
            let expected_note_present = expected_transition == "arm";
            if state.note_present != expected_note_present {
                return Err(StateError::ValidationFailed(format!(
                    "production_override note_present must match arm transition: expected \
                     {expected_note_present}, got {}",
                    state.note_present
                )));
            }
            if state.decision != ProductionOverrideDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "production_override terminal decision must match derived transition: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(
    state: ProductionOverrideState,
) -> Result<ProductionOverrideState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_production_override_graph() -> Result<ProductionOverrideGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("production_override").await?;
    build_production_override_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_production_override_graph_with_ephemeral_sqlite(
) -> Result<ProductionOverrideGraph, String> {
    build_production_override_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_production_override_graph_with_database_path(
    db_path: &str,
) -> Result<ProductionOverrideGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_production_override_graph_with_checkpointer(checkpointer).await
}

async fn build_production_override_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<ProductionOverrideGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = production_override_state_schema();
    let builder = StateGraphBuilder::<ProductionOverrideState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: ProductionOverrideState| async move {
                emit_decision_node_event("production_override", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW_NOOP,
            |s: ProductionOverrideState| async move {
                emit_decision_node_event("production_override", ALLOW_NOOP, &s.identifier)?;
                let mut next = s;
                next.decision = ProductionOverrideDecision::AllowNoop;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW_NOOP, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ARM,
            |s: ProductionOverrideState| async move {
                emit_decision_node_event("production_override", ARM, &s.identifier)?;
                let mut next = s;
                next.decision = ProductionOverrideDecision::Arm;
                Ok::<_, NodeError>(next)
            },
            node_config(ARM, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            LOCK,
            |s: ProductionOverrideState| async move {
                emit_decision_node_event("production_override", LOCK, &s.identifier)?;
                let mut next = s;
                next.decision = ProductionOverrideDecision::Lock;
                Ok::<_, NodeError>(next)
            },
            node_config(LOCK, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &ProductionOverrideState| match expected_decision(s) {
                ProductionOverrideDecision::AllowNoop => ALLOW_NOOP.into(),
                ProductionOverrideDecision::Arm => ARM.into(),
                ProductionOverrideDecision::Lock => LOCK.into(),
                ProductionOverrideDecision::Unclassified => ALLOW_NOOP.into(),
            },
        )
        .add_edge(ALLOW_NOOP, END)
        .add_edge(ARM, END)
        .add_edge(LOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_production_override_decision_report(
    compiled: &ProductionOverrideGraph,
    state: ProductionOverrideState,
) -> Result<ProductionOverrideGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id(
        "production_override",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "production_override",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "production_override",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(ProductionOverrideGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: production_override_graph_topology(compiled)?,
    })
}

pub fn production_override_graph_topology(
    compiled: &ProductionOverrideGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("production_override", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::events::HookInput;

    fn evaluation(prompt: &str, prior_armed: bool) -> ProductionOverrideEvaluation {
        let input = HookInput {
            session_id: Some("production-override-test".to_string()),
            prompt: Some(prompt.to_string()),
            ..Default::default()
        };
        sentinel_application::hooks::production_override::evaluate(&input, prior_armed)
    }

    fn missing_prompt_evaluation(prior_armed: bool) -> ProductionOverrideEvaluation {
        let input = HookInput {
            session_id: Some("production-override-test".to_string()),
            prompt: None,
            ..Default::default()
        };
        sentinel_application::hooks::production_override::evaluate(&input, prior_armed)
    }

    #[tokio::test]
    async fn graph_authorizes_arm_transition() {
        let graph = build_production_override_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProductionOverrideState::from_evaluation(
            "prod-override-arm",
            &evaluation("production override - hotfix", false),
        );
        assert!(optional_hex_digest_present(&state.prompt_sha256));
        let run = run_production_override_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, ProductionOverrideDecision::Arm);
        assert!(run
            .production_override_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_prompt_noop_with_absent_prompt_hash() {
        let graph = build_production_override_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProductionOverrideState::from_evaluation(
            "prod-override-missing-prompt",
            &missing_prompt_evaluation(false),
        );
        assert_eq!(state.prompt_sha256, None);
        let run = run_production_override_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, ProductionOverrideDecision::AllowNoop);
        assert!(!run.state.target_armed);
    }

    #[tokio::test]
    async fn graph_authorizes_lock_transition() {
        let graph = build_production_override_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProductionOverrideState::from_evaluation(
            "prod-override-lock",
            &evaluation("production lock", true),
        );
        let run = run_production_override_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, ProductionOverrideDecision::Lock);
        assert!(!run.state.target_armed);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_lock_bypass() {
        let graph = build_production_override_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = ProductionOverrideState::from_evaluation(
            "prod-override-forged",
            &evaluation("production lock", true),
        );
        state.transition = "noop".to_string();
        state.target_armed = true;
        state.notice_required = false;
        let err = run_production_override_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("transition"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_prompt_without_prompt_digest() {
        let graph = build_production_override_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = ProductionOverrideState::from_evaluation(
            "prod-override-missing-digest",
            &evaluation("production override - hotfix", false),
        );
        state.prompt_sha256 = None;
        let err = run_production_override_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("prompt_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_prompt_with_extra_prompt_digest() {
        let graph = build_production_override_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = ProductionOverrideState::from_evaluation(
            "prod-override-extra-digest",
            &missing_prompt_evaluation(false),
        );
        state.prompt_sha256 = Some(
            sentinel_application::hooks::production_override::prompt_sha256(
                "production override - hotfix",
            ),
        );
        let err = run_production_override_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing-prompt"), "{err}");
    }
}
