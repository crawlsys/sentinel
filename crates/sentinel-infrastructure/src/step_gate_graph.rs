//! Graph-backed step gate authorization.
//!
//! Step tools are only allowed when they are anchored to a loaded step plan, an
//! active LangGraph workflow projection, and the prerequisite StepProof chain.
//! This graph authorizes those deterministic facts through durable LangGraph
//! checkpoints before the CLI permits the step tool call.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::step_gate::StepGateEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StepGateDecision {
    #[default]
    Unclassified,
    Allow,
    AllowFirstStep,
    AllowPrerequisiteProof,
    DenyMissingStepConfig,
    DenyStepNotDeclared,
    DenyMissingGraphWorkflow,
    DenyPrerequisiteNotCompleted,
    DenyMissingProofChain,
    DenyMissingStepProof,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepGateState {
    pub identifier: String,
    pub tool: Option<String>,
    pub tool_present: bool,
    pub step_tool: bool,
    pub skill_present: bool,
    pub skill_sha256: Option<String>,
    pub step_id_present: bool,
    pub step_id_sha256: Option<String>,
    pub step_config_loaded: bool,
    pub step_declared: bool,
    pub phase_id_present: bool,
    pub phase_id_sha256: Option<String>,
    pub target_step_id_present: bool,
    pub target_step_id_sha256: Option<String>,
    pub target_step_description_present: bool,
    pub target_step_description_sha256: Option<String>,
    pub prerequisite_present: bool,
    pub prerequisite_step_id_present: bool,
    pub prerequisite_step_id_sha256: Option<String>,
    pub prerequisite_description_present: bool,
    pub prerequisite_description_sha256: Option<String>,
    pub graph_workflow_present: bool,
    pub prerequisite_graph_completed: bool,
    pub proof_chain_present: bool,
    pub step_proof_present: bool,
    pub blocking_finding_count: u64,
    pub should_deny: bool,
    pub decision: StepGateDecision,
}

impl StepGateState {
    #[must_use]
    pub fn from_evaluation(identifier: impl Into<String>, evaluation: &StepGateEvaluation) -> Self {
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            tool_present: evaluation.tool_present,
            step_tool: evaluation.step_tool,
            skill_present: evaluation.skill.is_some(),
            skill_sha256: evaluation.skill.as_deref().map(sha256),
            step_id_present: evaluation.step_id.is_some(),
            step_id_sha256: evaluation.step_id.as_deref().map(sha256),
            step_config_loaded: evaluation.step_config_loaded,
            step_declared: evaluation.step_declared,
            phase_id_present: evaluation.phase_id.is_some(),
            phase_id_sha256: evaluation.phase_id.as_deref().map(sha256),
            target_step_id_present: evaluation.target_step_id.is_some(),
            target_step_id_sha256: evaluation.target_step_id.as_deref().map(sha256),
            target_step_description_present: evaluation.target_step_description.is_some(),
            target_step_description_sha256: evaluation
                .target_step_description
                .as_deref()
                .map(sha256),
            prerequisite_present: evaluation.prerequisite_present,
            prerequisite_step_id_present: evaluation.prerequisite_step_id.is_some(),
            prerequisite_step_id_sha256: evaluation.prerequisite_step_id.as_deref().map(sha256),
            prerequisite_description_present: evaluation.prerequisite_description.is_some(),
            prerequisite_description_sha256: evaluation
                .prerequisite_description
                .as_deref()
                .map(sha256),
            graph_workflow_present: evaluation.graph_workflow_present,
            prerequisite_graph_completed: evaluation.prerequisite_graph_completed,
            proof_chain_present: evaluation.proof_chain_present,
            step_proof_present: evaluation.step_proof_present,
            blocking_finding_count: u64::from(evaluation.should_deny),
            should_deny: evaluation.should_deny,
            decision: StepGateDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StepGateGraphRun {
    pub state: StepGateState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<StepGateState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct StepGateAuthorization {
    decision: StepGateDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl StepGateAuthorization {
    #[must_use]
    pub fn decision(&self) -> StepGateDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl StepGateGraphRun {
    #[must_use]
    pub fn step_gate_authorization(&self) -> Result<Option<StepGateAuthorization>, String> {
        if self.state.decision == StepGateDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "step_gate",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(StepGateAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const ALLOW_FIRST_STEP: &str = "allow_first_step";
const ALLOW_PREREQUISITE_PROOF: &str = "allow_prerequisite_proof";
const DENY_MISSING_STEP_CONFIG: &str = "deny_missing_step_config";
const DENY_STEP_NOT_DECLARED: &str = "deny_step_not_declared";
const DENY_MISSING_GRAPH_WORKFLOW: &str = "deny_missing_graph_workflow";
const DENY_PREREQUISITE_NOT_COMPLETED: &str = "deny_prerequisite_not_completed";
const DENY_MISSING_PROOF_CHAIN: &str = "deny_missing_proof_chain";
const DENY_MISSING_STEP_PROOF: &str = "deny_missing_step_proof";

pub type StepGateGraph = CompilationResult<StepGateState>;

#[must_use]
pub fn step_gate_decision_label(decision: StepGateDecision) -> &'static str {
    match decision {
        StepGateDecision::Unclassified => "unclassified",
        StepGateDecision::Allow => "allow",
        StepGateDecision::AllowFirstStep => "allow-first-step",
        StepGateDecision::AllowPrerequisiteProof => "allow-prerequisite-proof",
        StepGateDecision::DenyMissingStepConfig => "deny-missing-step-config",
        StepGateDecision::DenyStepNotDeclared => "deny-step-not-declared",
        StepGateDecision::DenyMissingGraphWorkflow => "deny-missing-graph-workflow",
        StepGateDecision::DenyPrerequisiteNotCompleted => "deny-prerequisite-not-completed",
        StepGateDecision::DenyMissingProofChain => "deny-missing-proof-chain",
        StepGateDecision::DenyMissingStepProof => "deny-missing-step-proof",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn expected_decision(state: &StepGateState) -> StepGateDecision {
    if !state.step_tool {
        StepGateDecision::Allow
    } else if !state.step_config_loaded {
        StepGateDecision::DenyMissingStepConfig
    } else if !state.step_declared {
        StepGateDecision::DenyStepNotDeclared
    } else if !state.graph_workflow_present {
        StepGateDecision::DenyMissingGraphWorkflow
    } else if !state.prerequisite_present {
        StepGateDecision::AllowFirstStep
    } else if !state.prerequisite_graph_completed {
        StepGateDecision::DenyPrerequisiteNotCompleted
    } else if !state.proof_chain_present {
        StepGateDecision::DenyMissingProofChain
    } else if state.step_proof_present {
        StepGateDecision::AllowPrerequisiteProof
    } else {
        StepGateDecision::DenyMissingStepProof
    }
}

fn decision_denies(decision: StepGateDecision) -> bool {
    matches!(
        decision,
        StepGateDecision::DenyMissingStepConfig
            | StepGateDecision::DenyStepNotDeclared
            | StepGateDecision::DenyMissingGraphWorkflow
            | StepGateDecision::DenyPrerequisiteNotCompleted
            | StepGateDecision::DenyMissingProofChain
            | StepGateDecision::DenyMissingStepProof
    )
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "step_gate")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn step_gate_state_schema() -> StateSchema<StepGateState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "tool_present",
                "step_tool",
                "skill_present",
                "skill_sha256",
                "step_id_present",
                "step_id_sha256",
                "step_config_loaded",
                "step_declared",
                "phase_id_present",
                "phase_id_sha256",
                "target_step_id_present",
                "target_step_id_sha256",
                "target_step_description_present",
                "target_step_description_sha256",
                "prerequisite_present",
                "prerequisite_step_id_present",
                "prerequisite_step_id_sha256",
                "prerequisite_description_present",
                "prerequisite_description_sha256",
                "graph_workflow_present",
                "prerequisite_graph_completed",
                "proof_chain_present",
                "step_proof_present",
                "blocking_finding_count",
                "should_deny",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tool": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
                        { "type": "null" }
                    ]
                },
                "tool_present": { "type": "boolean" },
                "step_tool": { "type": "boolean" },
                "skill_present": { "type": "boolean" },
                "skill_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "step_id_present": { "type": "boolean" },
                "step_id_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "step_config_loaded": { "type": "boolean" },
                "step_declared": { "type": "boolean" },
                "phase_id_present": { "type": "boolean" },
                "phase_id_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "target_step_id_present": { "type": "boolean" },
                "target_step_id_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "target_step_description_present": { "type": "boolean" },
                "target_step_description_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "prerequisite_present": { "type": "boolean" },
                "prerequisite_step_id_present": { "type": "boolean" },
                "prerequisite_step_id_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "prerequisite_description_present": { "type": "boolean" },
                "prerequisite_description_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "graph_workflow_present": { "type": "boolean" },
                "prerequisite_graph_completed": { "type": "boolean" },
                "proof_chain_present": { "type": "boolean" },
                "step_proof_present": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_deny": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "AllowFirstStep",
                        "AllowPrerequisiteProof",
                        "DenyMissingStepConfig",
                        "DenyStepNotDeclared",
                        "DenyMissingGraphWorkflow",
                        "DenyPrerequisiteNotCompleted",
                        "DenyMissingProofChain",
                        "DenyMissingStepProof"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "step_gate",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &StepGateState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if !state.tool_present && state.step_tool {
                return Err(StateError::ValidationFailed(
                    "step_gate step tool requires tool name".to_string(),
                ));
            }
            if state.step_tool {
                if !state.skill_present
                    || !optional_hex_digest_present(&state.skill_sha256)
                    || !state.step_id_present
                    || !optional_hex_digest_present(&state.step_id_sha256)
                {
                    return Err(StateError::ValidationFailed(
                        "step_gate step tools require skill and step id hashes".to_string(),
                    ));
                }
            } else if state.skill_present
                || state.skill_sha256.is_some()
                || state.step_id_present
                || state.step_id_sha256.is_some()
                || state.step_config_loaded
                || state.step_declared
                || state.phase_id_present
                || state.phase_id_sha256.is_some()
                || state.target_step_id_present
                || state.target_step_id_sha256.is_some()
                || state.target_step_description_present
                || state.target_step_description_sha256.is_some()
                || state.prerequisite_present
                || state.prerequisite_step_id_present
                || state.prerequisite_step_id_sha256.is_some()
                || state.prerequisite_description_present
                || state.prerequisite_description_sha256.is_some()
                || state.graph_workflow_present
                || state.prerequisite_graph_completed
                || state.proof_chain_present
                || state.step_proof_present
                || state.blocking_finding_count > 0
                || state.should_deny
            {
                return Err(StateError::ValidationFailed(
                    "step_gate non-step tool cannot carry step authority facts".to_string(),
                ));
            }

            if !state.step_config_loaded
                && (state.step_declared
                    || state.phase_id_present
                    || state.phase_id_sha256.is_some()
                    || state.target_step_id_present
                    || state.target_step_id_sha256.is_some()
                    || state.target_step_description_present
                    || state.target_step_description_sha256.is_some()
                    || state.prerequisite_present
                    || state.prerequisite_step_id_present
                    || state.prerequisite_step_id_sha256.is_some()
                    || state.prerequisite_description_present
                    || state.prerequisite_description_sha256.is_some()
                    || state.graph_workflow_present
                    || state.prerequisite_graph_completed
                    || state.proof_chain_present
                    || state.step_proof_present)
            {
                return Err(StateError::ValidationFailed(
                    "step_gate missing step config cannot carry plan/graph/proof facts".to_string(),
                ));
            }

            if state.step_declared {
                if !state.step_config_loaded
                    || !state.phase_id_present
                    || !optional_hex_digest_present(&state.phase_id_sha256)
                    || !state.target_step_id_present
                    || !optional_hex_digest_present(&state.target_step_id_sha256)
                    || !state.target_step_description_present
                    || !optional_hex_digest_present(&state.target_step_description_sha256)
                {
                    return Err(StateError::ValidationFailed(
                        "step_gate declared step requires phase, target id, and target description hashes"
                            .to_string(),
                    ));
                }
            } else if state.phase_id_present
                || state.phase_id_sha256.is_some()
                || state.target_step_id_present
                || state.target_step_id_sha256.is_some()
                || state.target_step_description_present
                || state.target_step_description_sha256.is_some()
                || state.prerequisite_present
                || state.prerequisite_step_id_present
                || state.prerequisite_step_id_sha256.is_some()
                || state.prerequisite_description_present
                || state.prerequisite_description_sha256.is_some()
                || state.graph_workflow_present
                || state.prerequisite_graph_completed
                || state.proof_chain_present
                || state.step_proof_present
            {
                return Err(StateError::ValidationFailed(
                    "step_gate undeclared step cannot carry plan/graph/proof facts".to_string(),
                ));
            }

            if state.prerequisite_present {
                if !state.step_declared
                    || !state.prerequisite_step_id_present
                    || !optional_hex_digest_present(&state.prerequisite_step_id_sha256)
                    || !state.prerequisite_description_present
                    || !optional_hex_digest_present(&state.prerequisite_description_sha256)
                {
                    return Err(StateError::ValidationFailed(
                        "step_gate prerequisite requires declared step and prerequisite hashes"
                            .to_string(),
                    ));
                }
            } else if state.prerequisite_step_id_present
                || state.prerequisite_step_id_sha256.is_some()
                || state.prerequisite_description_present
                || state.prerequisite_description_sha256.is_some()
                || state.prerequisite_graph_completed
                || state.proof_chain_present
                || state.step_proof_present
            {
                return Err(StateError::ValidationFailed(
                    "step_gate first step cannot carry prerequisite/proof facts".to_string(),
                ));
            }

            if !state.graph_workflow_present
                && (state.prerequisite_graph_completed
                    || state.proof_chain_present
                    || state.step_proof_present)
            {
                return Err(StateError::ValidationFailed(
                    "step_gate missing graph workflow cannot carry graph/proof completion facts"
                        .to_string(),
                ));
            }
            if state.prerequisite_graph_completed
                && (!state.graph_workflow_present || !state.prerequisite_present)
            {
                return Err(StateError::ValidationFailed(
                    "step_gate prerequisite graph completion requires graph workflow and prerequisite"
                        .to_string(),
                ));
            }
            if state.proof_chain_present && !state.prerequisite_graph_completed {
                return Err(StateError::ValidationFailed(
                    "step_gate proof chain is only consulted after graph prerequisite completion"
                        .to_string(),
                ));
            }
            if state.step_proof_present && !state.proof_chain_present {
                return Err(StateError::ValidationFailed(
                    "step_gate StepProof requires an active proof chain".to_string(),
                ));
            }

            let expected_decision = expected_decision(state);
            let expected_should_deny = decision_denies(expected_decision);
            if state.should_deny != expected_should_deny {
                return Err(StateError::ValidationFailed(format!(
                    "step_gate should_deny must match step policy: expected \
                     {expected_should_deny}, got {}",
                    state.should_deny
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_deny);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "step_gate blocking_finding_count must match should_deny: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != StepGateDecision::Unclassified
                && state.decision != expected_decision
            {
                return Err(StateError::ValidationFailed(format!(
                    "step_gate terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision, expected_decision
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: StepGateState) -> Result<StepGateState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_step_gate_graph() -> Result<StepGateGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("step_gate").await?;
    build_step_gate_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_step_gate_graph_with_ephemeral_sqlite() -> Result<StepGateGraph, String> {
    build_step_gate_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_step_gate_graph_with_database_path(db_path: &str) -> Result<StepGateGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_step_gate_graph_with_checkpointer(checkpointer).await
}

async fn build_step_gate_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<StepGateGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = step_gate_state_schema();
    let builder = StateGraphBuilder::<StepGateState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW_FIRST_STEP,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", ALLOW_FIRST_STEP, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::AllowFirstStep;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW_FIRST_STEP, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW_PREREQUISITE_PROOF,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", ALLOW_PREREQUISITE_PROOF, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::AllowPrerequisiteProof;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_PREREQUISITE_PROOF,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_MISSING_STEP_CONFIG,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", DENY_MISSING_STEP_CONFIG, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::DenyMissingStepConfig;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_MISSING_STEP_CONFIG,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_STEP_NOT_DECLARED,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", DENY_STEP_NOT_DECLARED, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::DenyStepNotDeclared;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_STEP_NOT_DECLARED,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_MISSING_GRAPH_WORKFLOW,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", DENY_MISSING_GRAPH_WORKFLOW, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::DenyMissingGraphWorkflow;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_MISSING_GRAPH_WORKFLOW,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_PREREQUISITE_NOT_COMPLETED,
            |s: StepGateState| async move {
                emit_decision_node_event(
                    "step_gate",
                    DENY_PREREQUISITE_NOT_COMPLETED,
                    &s.identifier,
                )?;
                let mut next = s;
                next.decision = StepGateDecision::DenyPrerequisiteNotCompleted;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_PREREQUISITE_NOT_COMPLETED,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_MISSING_PROOF_CHAIN,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", DENY_MISSING_PROOF_CHAIN, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::DenyMissingProofChain;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_MISSING_PROOF_CHAIN,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_MISSING_STEP_PROOF,
            |s: StepGateState| async move {
                emit_decision_node_event("step_gate", DENY_MISSING_STEP_PROOF, &s.identifier)?;
                let mut next = s;
                next.decision = StepGateDecision::DenyMissingStepProof;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_MISSING_STEP_PROOF,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &StepGateState| match expected_decision(s) {
            StepGateDecision::Allow => ALLOW.into(),
            StepGateDecision::AllowFirstStep => ALLOW_FIRST_STEP.into(),
            StepGateDecision::AllowPrerequisiteProof => ALLOW_PREREQUISITE_PROOF.into(),
            StepGateDecision::DenyMissingStepConfig => DENY_MISSING_STEP_CONFIG.into(),
            StepGateDecision::DenyStepNotDeclared => DENY_STEP_NOT_DECLARED.into(),
            StepGateDecision::DenyMissingGraphWorkflow => DENY_MISSING_GRAPH_WORKFLOW.into(),
            StepGateDecision::DenyPrerequisiteNotCompleted => {
                DENY_PREREQUISITE_NOT_COMPLETED.into()
            }
            StepGateDecision::DenyMissingProofChain => DENY_MISSING_PROOF_CHAIN.into(),
            StepGateDecision::DenyMissingStepProof => DENY_MISSING_STEP_PROOF.into(),
            StepGateDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(ALLOW_FIRST_STEP, END)
        .add_edge(ALLOW_PREREQUISITE_PROOF, END)
        .add_edge(DENY_MISSING_STEP_CONFIG, END)
        .add_edge(DENY_STEP_NOT_DECLARED, END)
        .add_edge(DENY_MISSING_GRAPH_WORKFLOW, END)
        .add_edge(DENY_PREREQUISITE_NOT_COMPLETED, END)
        .add_edge(DENY_MISSING_PROOF_CHAIN, END)
        .add_edge(DENY_MISSING_STEP_PROOF, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_step_gate_decision_report(
    compiled: &StepGateGraph,
    state: StepGateState,
) -> Result<StepGateGraphRun, String> {
    let thread_id =
        crate::decision_graph_store::run_thread_id("step_gate", &state.identifier, &state)?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "step_gate", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "step_gate",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(StepGateGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: step_gate_graph_topology(compiled)?,
    })
}

pub fn step_gate_graph_topology(compiled: &StepGateGraph) -> Result<DecisionGraphTopology, String> {
    topology("step_gate", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::step_gate::{
        StepGateDecision as AppDecision, StepGateEvaluation,
    };

    fn first_step_evaluation() -> StepGateEvaluation {
        StepGateEvaluation {
            tool: Some("mcp__skills__linear__step_1".to_string()),
            tool_present: true,
            step_tool: true,
            skill: Some("linear".to_string()),
            step_id: Some("1".to_string()),
            step_config_loaded: true,
            step_declared: true,
            phase_id: Some("claim".to_string()),
            target_step_id: Some("1".to_string()),
            target_step_description: Some("fetch ticket".to_string()),
            prerequisite_present: false,
            prerequisite_step_id: None,
            prerequisite_description: None,
            graph_workflow_present: true,
            prerequisite_graph_completed: false,
            proof_chain_present: false,
            step_proof_present: false,
            should_deny: false,
            decision: AppDecision::AllowFirstStep,
        }
    }

    fn missing_graph_evaluation() -> StepGateEvaluation {
        StepGateEvaluation {
            graph_workflow_present: false,
            should_deny: true,
            decision: AppDecision::DenyMissingGraphWorkflow,
            ..first_step_evaluation()
        }
    }

    fn missing_proof_evaluation() -> StepGateEvaluation {
        StepGateEvaluation {
            tool: Some("mcp__skills__linear__step_2".to_string()),
            step_id: Some("2".to_string()),
            target_step_id: Some("2".to_string()),
            target_step_description: Some("create branch".to_string()),
            prerequisite_present: true,
            prerequisite_step_id: Some("1".to_string()),
            prerequisite_description: Some("fetch ticket".to_string()),
            graph_workflow_present: true,
            prerequisite_graph_completed: true,
            proof_chain_present: true,
            step_proof_present: false,
            should_deny: true,
            decision: AppDecision::DenyMissingStepProof,
            ..first_step_evaluation()
        }
    }

    fn proof_allow_evaluation() -> StepGateEvaluation {
        StepGateEvaluation {
            step_proof_present: true,
            should_deny: false,
            decision: AppDecision::AllowPrerequisiteProof,
            ..missing_proof_evaluation()
        }
    }

    #[tokio::test]
    async fn graph_authorizes_first_step_allow() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let state = StepGateState::from_evaluation("step-gate-first", &first_step_evaluation());
        assert!(state
            .skill_sha256
            .as_deref()
            .is_some_and(hex_digest_present));
        assert!(state
            .target_step_description_sha256
            .as_deref()
            .is_some_and(hex_digest_present));
        let run = run_step_gate_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, StepGateDecision::AllowFirstStep);
        assert!(run
            .step_gate_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_graph_deny() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let state =
            StepGateState::from_evaluation("step-gate-missing-graph", &missing_graph_evaluation());
        let run = run_step_gate_decision_report(&graph, state).await.unwrap();
        assert_eq!(
            run.state.decision,
            StepGateDecision::DenyMissingGraphWorkflow
        );
    }

    #[tokio::test]
    async fn graph_authorizes_missing_step_proof_deny() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let state =
            StepGateState::from_evaluation("step-gate-missing-proof", &missing_proof_evaluation());
        let run = run_step_gate_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, StepGateDecision::DenyMissingStepProof);
    }

    #[tokio::test]
    async fn graph_authorizes_prerequisite_proof_allow() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let state = StepGateState::from_evaluation("step-gate-proof", &proof_allow_evaluation());
        let run = run_step_gate_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, StepGateDecision::AllowPrerequisiteProof);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_missing_proof_allow() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state =
            StepGateState::from_evaluation("step-gate-forged", &missing_proof_evaluation());
        state.should_deny = false;
        state.blocking_finding_count = 0;
        let err = run_step_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_deny"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_skill_hash_evidence() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state =
            StepGateState::from_evaluation("step-gate-missing-skill", &first_step_evaluation());
        state.skill_sha256 = None;
        let err = run_step_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("skill and step id hashes"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_target_description_hash_evidence() {
        let graph = build_step_gate_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state = StepGateState::from_evaluation(
            "step-gate-missing-description",
            &first_step_evaluation(),
        );
        state.target_step_description_sha256 = None;
        let err = run_step_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("target description hashes"), "{err}");
    }
}
