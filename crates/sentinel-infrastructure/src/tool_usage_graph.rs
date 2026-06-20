//! Graph-backed tool-usage gate authorization.
//!
//! The application hook reads the live transcript and native Claude TaskList
//! for an Edit/Write decision. This graph validates those collected authority
//! facts, routes to the exact terminal decision, and checkpoints the result
//! before the CLI can return the hook output.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::tool_usage_gate::{
    PlanState, ToolUsageDecision as AppToolUsageDecision, ToolUsageEvaluation,
};
use sentinel_domain::ReversibilityClass;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolUsageDecision {
    #[default]
    Unclassified,
    AllowNoTool,
    AllowTriviallyReversible,
    AllowA3Handoff,
    Allow,
    DenyMissingSessionId,
    DenyMissingTranscriptPath,
    DenyTranscriptAuthority,
    DenyTaskListAuthority,
    DenyMissingSequentialThinking,
    DenyMissingTaskList,
    DenyPlanInProgress,
    DenyMissingApprovedPlan,
    DenyMissingInProgressTask,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageState {
    pub identifier: String,
    pub tool: Option<String>,
    pub tool_present: bool,
    pub reversibility_class_present: bool,
    pub reversibility_class: Option<ReversibilityClass>,
    pub a3_enabled: bool,
    pub a3_handoff: bool,
    pub gate_required: bool,
    pub session_id_present: bool,
    pub transcript_path_present: bool,
    pub transcript_authority_read: bool,
    pub transcript_authority_error_present: bool,
    pub transcript_authority_error_sha256: Option<String>,
    pub sequential_thinking_used: bool,
    pub plan_state: PlanState,
    pub task_authority_read: bool,
    pub task_authority_error_present: bool,
    pub task_authority_error_sha256: Option<String>,
    pub task_count: u64,
    pub in_progress_task_present: bool,
    pub pending_task_hint_present: bool,
    pub pending_task_hint_sha256: Option<String>,
    pub should_deny: bool,
    pub decision: ToolUsageDecision,
}

impl ToolUsageState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &ToolUsageEvaluation,
    ) -> Self {
        let transcript_authority_error_sha256 =
            evaluation.transcript_authority_error.as_deref().map(sha256);
        let task_authority_error_sha256 = evaluation.task_authority_error.as_deref().map(sha256);
        let pending_task_hint_sha256 = evaluation.pending_task_hint.as_deref().map(sha256);
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            tool_present: evaluation.tool_present,
            reversibility_class_present: evaluation.reversibility_class.is_some(),
            reversibility_class: evaluation.reversibility_class,
            a3_enabled: evaluation.a3_enabled,
            a3_handoff: evaluation.a3_handoff,
            gate_required: evaluation.gate_required,
            session_id_present: evaluation.session_id_present,
            transcript_path_present: evaluation.transcript_path_present,
            transcript_authority_read: evaluation.transcript_authority_read,
            transcript_authority_error_present: evaluation.transcript_authority_error.is_some(),
            transcript_authority_error_sha256,
            sequential_thinking_used: evaluation.sequential_thinking_used,
            plan_state: evaluation.plan_state,
            task_authority_read: evaluation.task_authority_read,
            task_authority_error_present: evaluation.task_authority_error.is_some(),
            task_authority_error_sha256,
            task_count: evaluation.task_count as u64,
            in_progress_task_present: evaluation.in_progress_task_present,
            pending_task_hint_present: evaluation.pending_task_hint.is_some(),
            pending_task_hint_sha256,
            should_deny: evaluation.should_deny,
            decision: ToolUsageDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolUsageGraphRun {
    pub state: ToolUsageState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<ToolUsageState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct ToolUsageAuthorization {
    decision: ToolUsageDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl ToolUsageAuthorization {
    #[must_use]
    pub fn decision(&self) -> ToolUsageDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl ToolUsageGraphRun {
    #[must_use]
    pub fn tool_usage_authorization(&self) -> Result<Option<ToolUsageAuthorization>, String> {
        if self.state.decision == ToolUsageDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "tool_usage",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(ToolUsageAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW_NO_TOOL: &str = "allow_no_tool";
const ALLOW_TRIVIALLY_REVERSIBLE: &str = "allow_trivially_reversible";
const ALLOW_A3_HANDOFF: &str = "allow_a3_handoff";
const ALLOW: &str = "allow";
const DENY_MISSING_SESSION_ID: &str = "deny_missing_session_id";
const DENY_MISSING_TRANSCRIPT_PATH: &str = "deny_missing_transcript_path";
const DENY_TRANSCRIPT_AUTHORITY: &str = "deny_transcript_authority";
const DENY_TASK_LIST_AUTHORITY: &str = "deny_task_list_authority";
const DENY_MISSING_SEQUENTIAL_THINKING: &str = "deny_missing_sequential_thinking";
const DENY_MISSING_TASK_LIST: &str = "deny_missing_task_list";
const DENY_PLAN_IN_PROGRESS: &str = "deny_plan_in_progress";
const DENY_MISSING_APPROVED_PLAN: &str = "deny_missing_approved_plan";
const DENY_MISSING_IN_PROGRESS_TASK: &str = "deny_missing_in_progress_task";

pub type ToolUsageGraph = CompilationResult<ToolUsageState>;

#[must_use]
pub fn tool_usage_decision_label(decision: ToolUsageDecision) -> &'static str {
    match decision {
        ToolUsageDecision::Unclassified => "unclassified",
        ToolUsageDecision::AllowNoTool => "allow-no-tool",
        ToolUsageDecision::AllowTriviallyReversible => "allow-trivially-reversible",
        ToolUsageDecision::AllowA3Handoff => "allow-a3-handoff",
        ToolUsageDecision::Allow => "allow",
        ToolUsageDecision::DenyMissingSessionId => "deny-missing-session-id",
        ToolUsageDecision::DenyMissingTranscriptPath => "deny-missing-transcript-path",
        ToolUsageDecision::DenyTranscriptAuthority => "deny-transcript-authority",
        ToolUsageDecision::DenyTaskListAuthority => "deny-task-list-authority",
        ToolUsageDecision::DenyMissingSequentialThinking => "deny-missing-sequential-thinking",
        ToolUsageDecision::DenyMissingTaskList => "deny-missing-task-list",
        ToolUsageDecision::DenyPlanInProgress => "deny-plan-in-progress",
        ToolUsageDecision::DenyMissingApprovedPlan => "deny-missing-approved-plan",
        ToolUsageDecision::DenyMissingInProgressTask => "deny-missing-in-progress-task",
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

fn expected_gate_required(state: &ToolUsageState) -> bool {
    let Some(class) = state.reversibility_class else {
        return false;
    };
    state.tool_present
        && class != ReversibilityClass::TriviallyReversible
        && !(state.a3_enabled
            && matches!(
                class,
                ReversibilityClass::Irreversible | ReversibilityClass::Catastrophic
            ))
}

fn expected_a3_handoff(state: &ToolUsageState) -> bool {
    matches!(
        state.reversibility_class,
        Some(ReversibilityClass::Irreversible | ReversibilityClass::Catastrophic)
    ) && state.a3_enabled
}

fn expected_decision(state: &ToolUsageState) -> ToolUsageDecision {
    let Some(class) = state.reversibility_class else {
        return ToolUsageDecision::AllowNoTool;
    };
    if !state.tool_present {
        return ToolUsageDecision::AllowNoTool;
    }
    if class == ReversibilityClass::TriviallyReversible {
        return ToolUsageDecision::AllowTriviallyReversible;
    }
    if expected_a3_handoff(state) {
        return ToolUsageDecision::AllowA3Handoff;
    }
    if !state.session_id_present {
        return ToolUsageDecision::DenyMissingSessionId;
    }
    if !state.transcript_path_present {
        return ToolUsageDecision::DenyMissingTranscriptPath;
    }
    if state.transcript_authority_error_present {
        return ToolUsageDecision::DenyTranscriptAuthority;
    }
    if state.task_authority_error_present {
        return ToolUsageDecision::DenyTaskListAuthority;
    }
    if !state.sequential_thinking_used {
        return ToolUsageDecision::DenyMissingSequentialThinking;
    }
    if state.task_count == 0 {
        return ToolUsageDecision::DenyMissingTaskList;
    }
    match state.plan_state {
        PlanState::Approved => {}
        PlanState::InPlanMode => return ToolUsageDecision::DenyPlanInProgress,
        PlanState::Missing => return ToolUsageDecision::DenyMissingApprovedPlan,
    }
    if !state.in_progress_task_present {
        return ToolUsageDecision::DenyMissingInProgressTask;
    }
    ToolUsageDecision::Allow
}

fn expected_should_deny(state: &ToolUsageState) -> bool {
    matches!(
        expected_decision(state),
        ToolUsageDecision::DenyMissingSessionId
            | ToolUsageDecision::DenyMissingTranscriptPath
            | ToolUsageDecision::DenyTranscriptAuthority
            | ToolUsageDecision::DenyTaskListAuthority
            | ToolUsageDecision::DenyMissingSequentialThinking
            | ToolUsageDecision::DenyMissingTaskList
            | ToolUsageDecision::DenyPlanInProgress
            | ToolUsageDecision::DenyMissingApprovedPlan
            | ToolUsageDecision::DenyMissingInProgressTask
    )
}

fn terminal_node_id(decision: ToolUsageDecision) -> &'static str {
    match decision {
        ToolUsageDecision::Unclassified => ALLOW,
        ToolUsageDecision::AllowNoTool => ALLOW_NO_TOOL,
        ToolUsageDecision::AllowTriviallyReversible => ALLOW_TRIVIALLY_REVERSIBLE,
        ToolUsageDecision::AllowA3Handoff => ALLOW_A3_HANDOFF,
        ToolUsageDecision::Allow => ALLOW,
        ToolUsageDecision::DenyMissingSessionId => DENY_MISSING_SESSION_ID,
        ToolUsageDecision::DenyMissingTranscriptPath => DENY_MISSING_TRANSCRIPT_PATH,
        ToolUsageDecision::DenyTranscriptAuthority => DENY_TRANSCRIPT_AUTHORITY,
        ToolUsageDecision::DenyTaskListAuthority => DENY_TASK_LIST_AUTHORITY,
        ToolUsageDecision::DenyMissingSequentialThinking => DENY_MISSING_SEQUENTIAL_THINKING,
        ToolUsageDecision::DenyMissingTaskList => DENY_MISSING_TASK_LIST,
        ToolUsageDecision::DenyPlanInProgress => DENY_PLAN_IN_PROGRESS,
        ToolUsageDecision::DenyMissingApprovedPlan => DENY_MISSING_APPROVED_PLAN,
        ToolUsageDecision::DenyMissingInProgressTask => DENY_MISSING_IN_PROGRESS_TASK,
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "tool_usage")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn tool_usage_state_schema() -> StateSchema<ToolUsageState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "tool_present",
                "reversibility_class_present",
                "reversibility_class",
                "a3_enabled",
                "a3_handoff",
                "gate_required",
                "session_id_present",
                "transcript_path_present",
                "transcript_authority_read",
                "transcript_authority_error_present",
                "transcript_authority_error_sha256",
                "sequential_thinking_used",
                "plan_state",
                "task_authority_read",
                "task_authority_error_present",
                "task_authority_error_sha256",
                "task_count",
                "in_progress_task_present",
                "pending_task_hint_present",
                "pending_task_hint_sha256",
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
                "reversibility_class_present": { "type": "boolean" },
                "reversibility_class": {
                    "anyOf": [
                        { "type": "null" },
                        {
                            "type": "string",
                            "enum": [
                                "TriviallyReversible",
                                "ReversibleWithEffort",
                                "Irreversible",
                                "Catastrophic"
                            ]
                        }
                    ]
                },
                "a3_enabled": { "type": "boolean" },
                "a3_handoff": { "type": "boolean" },
                "gate_required": { "type": "boolean" },
                "session_id_present": { "type": "boolean" },
                "transcript_path_present": { "type": "boolean" },
                "transcript_authority_read": { "type": "boolean" },
                "transcript_authority_error_present": { "type": "boolean" },
                "transcript_authority_error_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "sequential_thinking_used": { "type": "boolean" },
                "plan_state": {
                    "type": "string",
                    "enum": ["Missing", "InPlanMode", "Approved"]
                },
                "task_authority_read": { "type": "boolean" },
                "task_authority_error_present": { "type": "boolean" },
                "task_authority_error_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "task_count": { "type": "integer", "minimum": 0 },
                "in_progress_task_present": { "type": "boolean" },
                "pending_task_hint_present": { "type": "boolean" },
                "pending_task_hint_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "should_deny": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "AllowNoTool",
                        "AllowTriviallyReversible",
                        "AllowA3Handoff",
                        "Allow",
                        "DenyMissingSessionId",
                        "DenyMissingTranscriptPath",
                        "DenyTranscriptAuthority",
                        "DenyTaskListAuthority",
                        "DenyMissingSequentialThinking",
                        "DenyMissingTaskList",
                        "DenyPlanInProgress",
                        "DenyMissingApprovedPlan",
                        "DenyMissingInProgressTask"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "tool_usage",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &ToolUsageState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if state.reversibility_class_present != state.reversibility_class.is_some() {
                return Err(StateError::ValidationFailed(
                    "tool_usage reversibility_class_present must match reversibility_class"
                        .to_string(),
                ));
            }
            if state.tool_present && state.reversibility_class.is_none() {
                return Err(StateError::ValidationFailed(
                    "tool_usage present tool requires reversibility class".to_string(),
                ));
            }
            if !state.tool_present
                && (state.reversibility_class.is_some()
                    || state.a3_handoff
                    || state.gate_required
                    || state.transcript_authority_read
                    || state.task_authority_read
                    || state.should_deny)
            {
                return Err(StateError::ValidationFailed(
                    "tool_usage missing tool cannot carry authority facts".to_string(),
                ));
            }
            let expected_gate_required = expected_gate_required(state);
            if state.gate_required != expected_gate_required {
                return Err(StateError::ValidationFailed(format!(
                    "tool_usage gate_required must match class policy: expected \
                     {expected_gate_required}, got {}",
                    state.gate_required
                )));
            }
            let expected_a3_handoff = expected_a3_handoff(state);
            if state.a3_handoff != expected_a3_handoff {
                return Err(StateError::ValidationFailed(format!(
                    "tool_usage a3_handoff must match class policy: expected \
                     {expected_a3_handoff}, got {}",
                    state.a3_handoff
                )));
            }
            if !state.gate_required
                && (state.transcript_authority_read
                    || state.task_authority_read
                    || state.transcript_authority_error_present
                    || state.task_authority_error_present
                    || state.should_deny)
            {
                return Err(StateError::ValidationFailed(
                    "tool_usage non-gated decision cannot carry authority failure facts"
                        .to_string(),
                ));
            }
            if state.transcript_authority_error_present {
                if state.transcript_authority_read {
                    return Err(StateError::ValidationFailed(
                        "tool_usage transcript cannot be both read and errored".to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.transcript_authority_error_sha256) {
                    return Err(StateError::ValidationFailed(
                        "tool_usage transcript error requires 64-character digest".to_string(),
                    ));
                }
            } else if state.transcript_authority_error_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "tool_usage transcript error digest without error".to_string(),
                ));
            }
            if state.task_authority_error_present {
                if state.task_authority_read {
                    return Err(StateError::ValidationFailed(
                        "tool_usage TaskList cannot be both read and errored".to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.task_authority_error_sha256) {
                    return Err(StateError::ValidationFailed(
                        "tool_usage TaskList error requires 64-character digest".to_string(),
                    ));
                }
            } else if state.task_authority_error_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "tool_usage TaskList error digest without error".to_string(),
                ));
            }
            if state.pending_task_hint_present {
                if state.task_count == 0 {
                    return Err(StateError::ValidationFailed(
                        "tool_usage pending hint requires at least one task".to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.pending_task_hint_sha256) {
                    return Err(StateError::ValidationFailed(
                        "tool_usage pending hint requires 64-character digest".to_string(),
                    ));
                }
            } else if state.pending_task_hint_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "tool_usage pending hint digest without hint".to_string(),
                ));
            }
            if state.in_progress_task_present && state.task_count == 0 {
                return Err(StateError::ValidationFailed(
                    "tool_usage in-progress task requires non-empty task list".to_string(),
                ));
            }
            if state.gate_required
                && state.session_id_present
                && state.transcript_path_present
                && !state.transcript_authority_error_present
                && !state.transcript_authority_read
            {
                return Err(StateError::ValidationFailed(
                    "tool_usage transcript authority must be read after session and path pass"
                        .to_string(),
                ));
            }
            if state.gate_required
                && state.session_id_present
                && state.transcript_path_present
                && !state.transcript_authority_error_present
                && !state.task_authority_error_present
                && !state.task_authority_read
            {
                return Err(StateError::ValidationFailed(
                    "tool_usage TaskList authority must be read after transcript passes"
                        .to_string(),
                ));
            }
            let expected_should_deny = expected_should_deny(state);
            if state.should_deny != expected_should_deny {
                return Err(StateError::ValidationFailed(format!(
                    "tool_usage should_deny must match policy: expected {expected_should_deny}, \
                     got {}",
                    state.should_deny
                )));
            }
            let expected_decision = expected_decision(state);
            if state.decision != ToolUsageDecision::Unclassified
                && state.decision != expected_decision
            {
                return Err(StateError::ValidationFailed(format!(
                    "tool_usage terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision, expected_decision
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: ToolUsageState) -> Result<ToolUsageState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

async fn terminal_node(
    state: ToolUsageState,
    decision: ToolUsageDecision,
) -> Result<ToolUsageState, NodeError> {
    let mut next = state;
    next.decision = decision;
    Ok(next)
}

pub async fn build_tool_usage_graph() -> Result<ToolUsageGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("tool_usage").await?;
    build_tool_usage_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_tool_usage_graph_with_ephemeral_sqlite() -> Result<ToolUsageGraph, String> {
    build_tool_usage_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_tool_usage_graph_with_database_path(
    db_path: &str,
) -> Result<ToolUsageGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_tool_usage_graph_with_checkpointer(checkpointer).await
}

async fn build_tool_usage_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<ToolUsageGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = tool_usage_state_schema();
    let builder = StateGraphBuilder::<ToolUsageState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", CLASSIFY, &s.identifier)?;
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
            ALLOW_NO_TOOL,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", ALLOW_NO_TOOL, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::AllowNoTool).await
            },
            node_config(
                ALLOW_NO_TOOL,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW_TRIVIALLY_REVERSIBLE,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", ALLOW_TRIVIALLY_REVERSIBLE, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::AllowTriviallyReversible).await
            },
            node_config(
                ALLOW_TRIVIALLY_REVERSIBLE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW_A3_HANDOFF,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", ALLOW_A3_HANDOFF, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::AllowA3Handoff).await
            },
            node_config(
                ALLOW_A3_HANDOFF,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", ALLOW, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::Allow).await
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
            DENY_MISSING_SESSION_ID,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", DENY_MISSING_SESSION_ID, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::DenyMissingSessionId).await
            },
            node_config(
                DENY_MISSING_SESSION_ID,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_MISSING_TRANSCRIPT_PATH,
            |s: ToolUsageState| async move {
                emit_decision_node_event(
                    "tool_usage",
                    DENY_MISSING_TRANSCRIPT_PATH,
                    &s.identifier,
                )?;
                terminal_node(s, ToolUsageDecision::DenyMissingTranscriptPath).await
            },
            node_config(
                DENY_MISSING_TRANSCRIPT_PATH,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_TRANSCRIPT_AUTHORITY,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", DENY_TRANSCRIPT_AUTHORITY, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::DenyTranscriptAuthority).await
            },
            node_config(
                DENY_TRANSCRIPT_AUTHORITY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_TASK_LIST_AUTHORITY,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", DENY_TASK_LIST_AUTHORITY, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::DenyTaskListAuthority).await
            },
            node_config(
                DENY_TASK_LIST_AUTHORITY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_MISSING_SEQUENTIAL_THINKING,
            |s: ToolUsageState| async move {
                emit_decision_node_event(
                    "tool_usage",
                    DENY_MISSING_SEQUENTIAL_THINKING,
                    &s.identifier,
                )?;
                terminal_node(s, ToolUsageDecision::DenyMissingSequentialThinking).await
            },
            node_config(
                DENY_MISSING_SEQUENTIAL_THINKING,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_MISSING_TASK_LIST,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", DENY_MISSING_TASK_LIST, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::DenyMissingTaskList).await
            },
            node_config(
                DENY_MISSING_TASK_LIST,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_PLAN_IN_PROGRESS,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", DENY_PLAN_IN_PROGRESS, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::DenyPlanInProgress).await
            },
            node_config(
                DENY_PLAN_IN_PROGRESS,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_MISSING_APPROVED_PLAN,
            |s: ToolUsageState| async move {
                emit_decision_node_event("tool_usage", DENY_MISSING_APPROVED_PLAN, &s.identifier)?;
                terminal_node(s, ToolUsageDecision::DenyMissingApprovedPlan).await
            },
            node_config(
                DENY_MISSING_APPROVED_PLAN,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            DENY_MISSING_IN_PROGRESS_TASK,
            |s: ToolUsageState| async move {
                emit_decision_node_event(
                    "tool_usage",
                    DENY_MISSING_IN_PROGRESS_TASK,
                    &s.identifier,
                )?;
                terminal_node(s, ToolUsageDecision::DenyMissingInProgressTask).await
            },
            node_config(
                DENY_MISSING_IN_PROGRESS_TASK,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &ToolUsageState| {
            terminal_node_id(expected_decision(s)).into()
        })
        .add_edge(ALLOW_NO_TOOL, END)
        .add_edge(ALLOW_TRIVIALLY_REVERSIBLE, END)
        .add_edge(ALLOW_A3_HANDOFF, END)
        .add_edge(ALLOW, END)
        .add_edge(DENY_MISSING_SESSION_ID, END)
        .add_edge(DENY_MISSING_TRANSCRIPT_PATH, END)
        .add_edge(DENY_TRANSCRIPT_AUTHORITY, END)
        .add_edge(DENY_TASK_LIST_AUTHORITY, END)
        .add_edge(DENY_MISSING_SEQUENTIAL_THINKING, END)
        .add_edge(DENY_MISSING_TASK_LIST, END)
        .add_edge(DENY_PLAN_IN_PROGRESS, END)
        .add_edge(DENY_MISSING_APPROVED_PLAN, END)
        .add_edge(DENY_MISSING_IN_PROGRESS_TASK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_tool_usage_decision_report(
    compiled: &ToolUsageGraph,
    state: ToolUsageState,
) -> Result<ToolUsageGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "tool_usage",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "tool_usage", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "tool_usage",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(ToolUsageGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: tool_usage_graph_topology(compiled)?,
    })
}

pub fn tool_usage_graph_topology(
    compiled: &ToolUsageGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("tool_usage", compiled)
}

#[must_use]
pub fn expected_decision_from_app(evaluation: &ToolUsageEvaluation) -> ToolUsageDecision {
    match evaluation.decision {
        AppToolUsageDecision::AllowNoTool => ToolUsageDecision::AllowNoTool,
        AppToolUsageDecision::AllowTriviallyReversible => {
            ToolUsageDecision::AllowTriviallyReversible
        }
        AppToolUsageDecision::AllowA3Handoff => ToolUsageDecision::AllowA3Handoff,
        AppToolUsageDecision::Allow => ToolUsageDecision::Allow,
        AppToolUsageDecision::DenyMissingSessionId => ToolUsageDecision::DenyMissingSessionId,
        AppToolUsageDecision::DenyMissingTranscriptPath => {
            ToolUsageDecision::DenyMissingTranscriptPath
        }
        AppToolUsageDecision::DenyTranscriptAuthority => ToolUsageDecision::DenyTranscriptAuthority,
        AppToolUsageDecision::DenyTaskListAuthority => ToolUsageDecision::DenyTaskListAuthority,
        AppToolUsageDecision::DenyMissingSequentialThinking => {
            ToolUsageDecision::DenyMissingSequentialThinking
        }
        AppToolUsageDecision::DenyMissingTaskList => ToolUsageDecision::DenyMissingTaskList,
        AppToolUsageDecision::DenyPlanInProgress => ToolUsageDecision::DenyPlanInProgress,
        AppToolUsageDecision::DenyMissingApprovedPlan => ToolUsageDecision::DenyMissingApprovedPlan,
        AppToolUsageDecision::DenyMissingInProgressTask => {
            ToolUsageDecision::DenyMissingInProgressTask
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_state(decision: AppToolUsageDecision) -> ToolUsageState {
        let evaluation = ToolUsageEvaluation {
            tool: Some("Edit".to_string()),
            tool_present: true,
            reversibility_class: Some(ReversibilityClass::ReversibleWithEffort),
            a3_enabled: false,
            a3_handoff: false,
            gate_required: true,
            session_id: Some("sess".to_string()),
            session_id_present: true,
            transcript_path: Some("/tmp/transcript.jsonl".to_string()),
            transcript_path_present: true,
            transcript_authority_read: true,
            transcript_authority_error: None,
            sequential_thinking_used: true,
            plan_state: PlanState::Approved,
            task_authority_read: true,
            task_authority_error: None,
            task_count: 1,
            in_progress_task_present: true,
            pending_task_hint: None,
            should_deny: false,
            decision,
        };
        ToolUsageState::from_evaluation("tool-usage-test", &evaluation)
    }

    #[tokio::test]
    async fn graph_authorizes_allow() {
        let graph = build_tool_usage_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = base_state(AppToolUsageDecision::Allow);
        assert_eq!(state.tool.as_deref(), Some("Edit"));
        assert!(state.transcript_authority_error_sha256.is_none());
        let run = run_tool_usage_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, ToolUsageDecision::Allow);
        assert!(run
            .tool_usage_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_transcript_deny() {
        let graph = build_tool_usage_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = base_state(AppToolUsageDecision::DenyMissingTranscriptPath);
        state.transcript_path_present = false;
        state.transcript_authority_read = false;
        state.task_authority_read = false;
        state.sequential_thinking_used = false;
        state.plan_state = PlanState::Missing;
        state.task_count = 0;
        state.in_progress_task_present = false;
        state.should_deny = true;
        let run = run_tool_usage_decision_report(&graph, state).await.unwrap();
        assert_eq!(
            run.state.decision,
            ToolUsageDecision::DenyMissingTranscriptPath
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_without_sequential_thinking() {
        let graph = build_tool_usage_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = base_state(AppToolUsageDecision::Allow);
        state.sequential_thinking_used = false;
        state.should_deny = false;
        let err = run_tool_usage_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_deny"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_transcript_error_digest() {
        let graph = build_tool_usage_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = base_state(AppToolUsageDecision::DenyTranscriptAuthority);
        state.transcript_authority_read = false;
        state.transcript_authority_error_present = true;
        state.transcript_authority_error_sha256 = None;
        state.task_authority_read = false;
        state.sequential_thinking_used = false;
        state.plan_state = PlanState::Missing;
        state.task_count = 0;
        state.in_progress_task_present = false;
        state.should_deny = true;
        let err = run_tool_usage_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("transcript error requires"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_pending_hint_without_digest() {
        let graph = build_tool_usage_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = base_state(AppToolUsageDecision::Allow);
        state.pending_task_hint_present = true;
        state.pending_task_hint_sha256 = None;
        let err = run_tool_usage_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("pending hint requires"), "{err}");
    }
}
