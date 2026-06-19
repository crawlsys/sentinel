//! Graph-backed phase-gate hook authorization.
//!
//! The phase gate is intentionally side-effecting: it records tool-call counts,
//! phase-file reads, and phase-file hashes before deciding whether a tool call
//! may continue. This graph authorizes that hook-boundary decision and the
//! observed state deltas through durable LangGraph checkpoints.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::phase_gate::{
    PhaseGateDecision as AppPhaseGateDecision, PhaseGateEvaluation,
};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PhaseGateDecision {
    #[default]
    Unclassified,
    Allow,
    Block,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseGateState {
    pub identifier: String,
    pub tool: Option<String>,
    pub tool_present: bool,
    pub dangerous_mcp_tool: bool,
    pub safe_mcp_tool: bool,
    pub tool_calls_before: u64,
    pub tool_calls_after: u64,
    pub tool_call_recorded: bool,
    pub phases_read_before: u64,
    pub phases_read_after: u64,
    pub phase_read_recorded: bool,
    pub phase_hashes_before: u64,
    pub phase_hashes_after: u64,
    pub phase_hash_recorded: bool,
    pub blocked: bool,
    pub reason_present: bool,
    pub reason_sha256: Option<String>,
    pub reason_len: u64,
    pub evaluated_decision: PhaseGateDecision,
    pub decision: PhaseGateDecision,
}

impl PhaseGateState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &PhaseGateEvaluation,
    ) -> Self {
        let reason_sha256 = evaluation
            .reason_present
            .then(|| evaluation.reason_sha256.clone());
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            tool_present: evaluation.tool_present,
            dangerous_mcp_tool: evaluation.dangerous_mcp_tool,
            safe_mcp_tool: evaluation.safe_mcp_tool,
            tool_calls_before: evaluation.tool_calls_before,
            tool_calls_after: evaluation.tool_calls_after,
            tool_call_recorded: evaluation.tool_call_recorded,
            phases_read_before: evaluation.phases_read_before as u64,
            phases_read_after: evaluation.phases_read_after as u64,
            phase_read_recorded: evaluation.phase_read_recorded,
            phase_hashes_before: evaluation.phase_hashes_before as u64,
            phase_hashes_after: evaluation.phase_hashes_after as u64,
            phase_hash_recorded: evaluation.phase_hash_recorded,
            blocked: evaluation.blocked,
            reason_present: evaluation.reason_present,
            reason_sha256,
            reason_len: evaluation.reason_len as u64,
            evaluated_decision: phase_gate_decision_from_app(evaluation.decision),
            decision: PhaseGateDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseGateGraphRun {
    pub state: PhaseGateState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<PhaseGateState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct PhaseGateAuthorization {
    decision: PhaseGateDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl PhaseGateAuthorization {
    #[must_use]
    pub fn decision(&self) -> PhaseGateDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl PhaseGateGraphRun {
    #[must_use]
    pub fn phase_gate_authorization(&self) -> Result<Option<PhaseGateAuthorization>, String> {
        if self.state.decision == PhaseGateDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "phase_gate",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(PhaseGateAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK: &str = "block";
const DENY: &str = "deny";

pub type PhaseGateGraph = CompilationResult<PhaseGateState>;

#[must_use]
pub fn phase_gate_decision_label(decision: PhaseGateDecision) -> &'static str {
    match decision {
        PhaseGateDecision::Unclassified => "unclassified",
        PhaseGateDecision::Allow => "allow",
        PhaseGateDecision::Block => "block",
        PhaseGateDecision::Deny => "deny",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

#[must_use]
pub fn phase_gate_decision_from_app(decision: AppPhaseGateDecision) -> PhaseGateDecision {
    match decision {
        AppPhaseGateDecision::Allow => PhaseGateDecision::Allow,
        AppPhaseGateDecision::Block => PhaseGateDecision::Block,
        AppPhaseGateDecision::Deny => PhaseGateDecision::Deny,
    }
}

#[must_use]
pub fn expected_decision_from_app(evaluation: &PhaseGateEvaluation) -> PhaseGateDecision {
    phase_gate_decision_from_app(evaluation.decision)
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn terminal_node_id(decision: PhaseGateDecision) -> &'static str {
    match decision {
        PhaseGateDecision::Unclassified | PhaseGateDecision::Allow => ALLOW,
        PhaseGateDecision::Block => BLOCK,
        PhaseGateDecision::Deny => DENY,
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "phase_gate")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn phase_gate_state_schema() -> StateSchema<PhaseGateState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "tool_present",
                "dangerous_mcp_tool",
                "safe_mcp_tool",
                "tool_calls_before",
                "tool_calls_after",
                "tool_call_recorded",
                "phases_read_before",
                "phases_read_after",
                "phase_read_recorded",
                "phase_hashes_before",
                "phase_hashes_after",
                "phase_hash_recorded",
                "blocked",
                "reason_present",
                "reason_sha256",
                "reason_len",
                "evaluated_decision",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "tool": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "tool_present": { "type": "boolean" },
                "dangerous_mcp_tool": { "type": "boolean" },
                "safe_mcp_tool": { "type": "boolean" },
                "tool_calls_before": { "type": "integer", "minimum": 0 },
                "tool_calls_after": { "type": "integer", "minimum": 0 },
                "tool_call_recorded": { "type": "boolean" },
                "phases_read_before": { "type": "integer", "minimum": 0 },
                "phases_read_after": { "type": "integer", "minimum": 0 },
                "phase_read_recorded": { "type": "boolean" },
                "phase_hashes_before": { "type": "integer", "minimum": 0 },
                "phase_hashes_after": { "type": "integer", "minimum": 0 },
                "phase_hash_recorded": { "type": "boolean" },
                "blocked": { "type": "boolean" },
                "reason_present": { "type": "boolean" },
                "reason_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "reason_len": { "type": "integer", "minimum": 0 },
                "evaluated_decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block", "Deny"]
                },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Block", "Deny"]
                }
            },
            "x-sentinel": {
                "graph": "phase_gate",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &PhaseGateState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if state.safe_mcp_tool && state.dangerous_mcp_tool {
                return Err(StateError::ValidationFailed(
                    "phase_gate MCP tool cannot be both safe and dangerous".to_string(),
                ));
            }
            if !state.tool_present
                && (state.safe_mcp_tool
                    || state.dangerous_mcp_tool
                    || state.tool_call_recorded
                    || state.phase_read_recorded
                    || state.phase_hash_recorded
                    || state.blocked
                    || state.reason_present)
            {
                return Err(StateError::ValidationFailed(
                    "phase_gate missing tool cannot carry decision facts".to_string(),
                ));
            }
            if state.tool_calls_after < state.tool_calls_before {
                return Err(StateError::ValidationFailed(
                    "phase_gate tool_calls_after cannot be less than before".to_string(),
                ));
            }
            let tool_delta = state.tool_calls_after - state.tool_calls_before;
            if tool_delta > 1 {
                return Err(StateError::ValidationFailed(
                    "phase_gate tool call delta must be at most one".to_string(),
                ));
            }
            if state.tool_call_recorded != (tool_delta > 0) {
                return Err(StateError::ValidationFailed(
                    "phase_gate tool_call_recorded must match tool-call delta".to_string(),
                ));
            }
            let expected_tool_call_recorded = state.tool_present && !state.safe_mcp_tool;
            if state.tool_call_recorded != expected_tool_call_recorded {
                return Err(StateError::ValidationFailed(format!(
                    "phase_gate tool call recording must match policy: expected \
                     {expected_tool_call_recorded}, got {}",
                    state.tool_call_recorded
                )));
            }
            if state.phases_read_after < state.phases_read_before {
                return Err(StateError::ValidationFailed(
                    "phase_gate phases_read_after cannot be less than before".to_string(),
                ));
            }
            if state.phase_read_recorded != (state.phases_read_after > state.phases_read_before) {
                return Err(StateError::ValidationFailed(
                    "phase_gate phase_read_recorded must match phase-read delta".to_string(),
                ));
            }
            if state.phase_hashes_after < state.phase_hashes_before {
                return Err(StateError::ValidationFailed(
                    "phase_gate phase_hashes_after cannot be less than before".to_string(),
                ));
            }
            if state.phase_hash_recorded != (state.phase_hashes_after > state.phase_hashes_before) {
                return Err(StateError::ValidationFailed(
                    "phase_gate phase_hash_recorded must match phase-hash delta".to_string(),
                ));
            }
            if state.reason_present {
                if state.reason_len == 0 || !optional_hex_digest_present(&state.reason_sha256) {
                    return Err(StateError::ValidationFailed(
                        "phase_gate reason requires non-empty length and 64-character digest"
                            .to_string(),
                    ));
                }
            } else if state.reason_len != 0 || state.reason_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "phase_gate reason digest/length without reason".to_string(),
                ));
            }
            match state.evaluated_decision {
                PhaseGateDecision::Unclassified => {
                    return Err(StateError::ValidationFailed(
                        "phase_gate evaluated decision cannot be unclassified".to_string(),
                    ));
                }
                PhaseGateDecision::Allow => {
                    if state.blocked || state.reason_present {
                        return Err(StateError::ValidationFailed(
                            "phase_gate allow cannot be blocked or carry a block reason"
                                .to_string(),
                        ));
                    }
                }
                PhaseGateDecision::Block | PhaseGateDecision::Deny => {
                    if !state.blocked || !state.reason_present {
                        return Err(StateError::ValidationFailed(
                            "phase_gate blocking decision requires blocked output and reason"
                                .to_string(),
                        ));
                    }
                }
            }
            if state.decision != PhaseGateDecision::Unclassified
                && state.decision != state.evaluated_decision
            {
                return Err(StateError::ValidationFailed(format!(
                    "phase_gate terminal decision must match evaluated decision: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision, state.evaluated_decision
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: PhaseGateState) -> Result<PhaseGateState, NodeError> {
    let mut next = state;
    next.decision = next.evaluated_decision;
    Ok(next)
}

async fn terminal_node(
    state: PhaseGateState,
    decision: PhaseGateDecision,
) -> Result<PhaseGateState, NodeError> {
    let mut next = state;
    next.decision = decision;
    Ok(next)
}

pub async fn build_phase_gate_graph() -> Result<PhaseGateGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("phase_gate").await?;
    build_phase_gate_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_phase_gate_graph_with_ephemeral_sqlite() -> Result<PhaseGateGraph, String> {
    build_phase_gate_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_phase_gate_graph_with_database_path(
    db_path: &str,
) -> Result<PhaseGateGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_phase_gate_graph_with_checkpointer(checkpointer).await
}

async fn build_phase_gate_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<PhaseGateGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = phase_gate_state_schema();
    let builder = StateGraphBuilder::<PhaseGateState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: PhaseGateState| async move {
                emit_decision_node_event("phase_gate", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: PhaseGateState| async move {
                emit_decision_node_event("phase_gate", ALLOW, &s.identifier)?;
                terminal_node(s, PhaseGateDecision::Allow).await
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            BLOCK,
            |s: PhaseGateState| async move {
                emit_decision_node_event("phase_gate", BLOCK, &s.identifier)?;
                terminal_node(s, PhaseGateDecision::Block).await
            },
            node_config(BLOCK, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            DENY,
            |s: PhaseGateState| async move {
                emit_decision_node_event("phase_gate", DENY, &s.identifier)?;
                terminal_node(s, PhaseGateDecision::Deny).await
            },
            node_config(DENY, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &PhaseGateState| {
            terminal_node_id(s.evaluated_decision).into()
        })
        .add_edge(ALLOW, END)
        .add_edge(BLOCK, END)
        .add_edge(DENY, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_phase_gate_decision_report(
    compiled: &PhaseGateGraph,
    state: PhaseGateState,
) -> Result<PhaseGateGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "phase_gate",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "phase_gate", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "phase_gate",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(PhaseGateGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: phase_gate_graph_topology(compiled)?,
    })
}

pub fn phase_gate_graph_topology(
    compiled: &PhaseGateGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("phase_gate", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow_state() -> PhaseGateState {
        PhaseGateState {
            identifier: "phase-gate-allow".to_string(),
            tool: Some("Edit".to_string()),
            tool_present: true,
            dangerous_mcp_tool: false,
            safe_mcp_tool: false,
            tool_calls_before: 2,
            tool_calls_after: 3,
            tool_call_recorded: true,
            phases_read_before: 1,
            phases_read_after: 1,
            phase_read_recorded: false,
            phase_hashes_before: 1,
            phase_hashes_after: 1,
            phase_hash_recorded: false,
            blocked: false,
            reason_present: false,
            reason_sha256: None,
            reason_len: 0,
            evaluated_decision: PhaseGateDecision::Allow,
            decision: PhaseGateDecision::Unclassified,
        }
    }

    fn deny_state() -> PhaseGateState {
        let reason = "blocked by phase gate";
        PhaseGateState {
            blocked: true,
            reason_present: true,
            reason_sha256: Some(sha256(reason)),
            reason_len: reason.len() as u64,
            evaluated_decision: PhaseGateDecision::Deny,
            ..allow_state()
        }
    }

    #[tokio::test]
    async fn graph_authorizes_allow() {
        let graph = build_phase_gate_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = allow_state();
        assert_eq!(state.tool.as_deref(), Some("Edit"));
        assert_eq!(state.reason_sha256, None);
        let run = run_phase_gate_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, PhaseGateDecision::Allow);
        assert!(run
            .phase_gate_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_deny() {
        let graph = build_phase_gate_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = deny_state();
        assert!(optional_hex_digest_present(&state.reason_sha256));
        let run = run_phase_gate_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, PhaseGateDecision::Deny);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_with_blocked_output() {
        let graph = build_phase_gate_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = deny_state();
        state.evaluated_decision = PhaseGateDecision::Allow;
        let err = run_phase_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("allow"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_tool_identity() {
        let graph = build_phase_gate_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = allow_state();
        state.tool = None;
        let err = run_phase_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("tool identity"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_reason_without_reason_digest() {
        let graph = build_phase_gate_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = deny_state();
        state.reason_sha256 = None;
        let err = run_phase_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("reason"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_absent_reason_with_extra_reason_digest() {
        let graph = build_phase_gate_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = allow_state();
        state.reason_sha256 = Some(sha256("unexpected reason"));
        let err = run_phase_gate_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("without reason"), "{err}");
    }
}
