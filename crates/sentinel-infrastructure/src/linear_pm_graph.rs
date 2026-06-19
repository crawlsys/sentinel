//! Graph-backed Linear PM gate authorization.
//!
//! The application hook computes deterministic facts for Linear start
//! transitions: live issue lookup, blocked-ticket evidence, oversized-ticket
//! policy, milestone discipline, and priority cherry-pick detection. This graph
//! authorizes the ordered allow/block decision through durable LangGraph
//! checkpoints before the CLI permits a Linear state transition.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::linear_pm_gate::LinearPmEvaluation;
use sentinel_application::linear_pm_audit::OVERSIZED_POINTS;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

const TARGET_TOOL: &str = "mcp__linear__update_issue";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LinearPmDecision {
    #[default]
    Unclassified,
    Allow,
    BlockMissingIssueIdentifier,
    BlockLiveAuthorityUnavailable,
    BlockBlockedTicket,
    BlockOversizedTicket,
    BlockMissingMilestone,
    BlockHigherPriorityAvailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinearPmState {
    pub identifier: String,
    pub tool: Option<String>,
    pub target_tool: bool,
    pub tool_input_present: bool,
    pub start_transition: bool,
    pub issue_key_present: bool,
    pub issue_key_sha256: Option<String>,
    pub issue_fetched: bool,
    pub live_authority_error_present: bool,
    pub live_authority_error_sha256: Option<String>,
    pub ticket_identifier_present: bool,
    pub ticket_identifier_sha256: Option<String>,
    pub blocked_ticket: bool,
    pub blocked_reason_present: bool,
    pub blocked_reason_sha256: Option<String>,
    pub estimate_present: bool,
    pub estimate_points: f64,
    pub oversized_ticket: bool,
    pub project_has_milestones: bool,
    pub milestone_present: bool,
    pub missing_milestone: bool,
    pub target_priority_present: bool,
    pub target_priority: i64,
    pub target_assignee_present: bool,
    pub higher_priority_available: bool,
    pub higher_priority_ticket_present: bool,
    pub higher_priority_ticket_sha256: Option<String>,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: LinearPmDecision,
}

impl LinearPmState {
    #[must_use]
    pub fn from_evaluation(identifier: impl Into<String>, evaluation: &LinearPmEvaluation) -> Self {
        let tool = evaluation
            .tool
            .as_deref()
            .map(str::trim)
            .filter(|tool| !tool.is_empty())
            .map(ToString::to_string);
        let issue_key_sha256 = evaluation
            .issue_key
            .as_deref()
            .map(str::trim)
            .filter(|issue_key| !issue_key.is_empty() && evaluation.issue_key_present)
            .map(sha256);
        let live_authority_error_sha256 = evaluation
            .live_authority_error
            .as_deref()
            .map(str::trim)
            .filter(|error| !error.is_empty())
            .map(sha256);
        let ticket_identifier_sha256 = evaluation
            .ticket_identifier
            .as_deref()
            .map(str::trim)
            .filter(|ticket| !ticket.is_empty())
            .map(sha256);
        let blocked_reason_sha256 = evaluation
            .blocked_reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .map(sha256);
        let higher_priority_ticket_sha256 = evaluation
            .higher_priority_ticket
            .as_deref()
            .map(str::trim)
            .filter(|ticket| !ticket.is_empty())
            .map(sha256);
        Self {
            identifier: identifier.into(),
            tool,
            target_tool: evaluation.target_tool,
            tool_input_present: evaluation.tool_input_present,
            start_transition: evaluation.start_transition,
            issue_key_present: evaluation.issue_key_present,
            issue_key_sha256,
            issue_fetched: evaluation.issue_fetched,
            live_authority_error_present: live_authority_error_sha256.is_some(),
            live_authority_error_sha256,
            ticket_identifier_present: ticket_identifier_sha256.is_some(),
            ticket_identifier_sha256,
            blocked_ticket: evaluation.blocked_ticket,
            blocked_reason_present: blocked_reason_sha256.is_some(),
            blocked_reason_sha256,
            estimate_present: evaluation.estimate_present,
            estimate_points: evaluation.estimate_points,
            oversized_ticket: evaluation.oversized_ticket,
            project_has_milestones: evaluation.project_has_milestones,
            milestone_present: evaluation.milestone_present,
            missing_milestone: evaluation.missing_milestone,
            target_priority_present: evaluation.target_priority_present,
            target_priority: evaluation.target_priority,
            target_assignee_present: evaluation.target_assignee_present,
            higher_priority_available: evaluation.higher_priority_available,
            higher_priority_ticket_present: higher_priority_ticket_sha256.is_some(),
            higher_priority_ticket_sha256,
            blocking_finding_count: u64::from(evaluation.should_block),
            should_block: evaluation.should_block,
            decision: LinearPmDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LinearPmGraphRun {
    pub state: LinearPmState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<LinearPmState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct LinearPmAuthorization {
    decision: LinearPmDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl LinearPmAuthorization {
    #[must_use]
    pub fn decision(&self) -> LinearPmDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl LinearPmGraphRun {
    #[must_use]
    pub fn linear_pm_authorization(&self) -> Result<Option<LinearPmAuthorization>, String> {
        if self.state.decision == LinearPmDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "linear_pm",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(LinearPmAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const BLOCK_MISSING_ISSUE_IDENTIFIER: &str = "block_missing_issue_identifier";
const BLOCK_LIVE_AUTHORITY_UNAVAILABLE: &str = "block_live_authority_unavailable";
const BLOCK_BLOCKED_TICKET: &str = "block_blocked_ticket";
const BLOCK_OVERSIZED_TICKET: &str = "block_oversized_ticket";
const BLOCK_MISSING_MILESTONE: &str = "block_missing_milestone";
const BLOCK_HIGHER_PRIORITY_AVAILABLE: &str = "block_higher_priority_available";

pub type LinearPmGraph = CompilationResult<LinearPmState>;

#[must_use]
pub fn linear_pm_decision_label(decision: LinearPmDecision) -> &'static str {
    match decision {
        LinearPmDecision::Unclassified => "unclassified",
        LinearPmDecision::Allow => "allow",
        LinearPmDecision::BlockMissingIssueIdentifier => "block-missing-issue-identifier",
        LinearPmDecision::BlockLiveAuthorityUnavailable => "block-live-authority-unavailable",
        LinearPmDecision::BlockBlockedTicket => "block-blocked-ticket",
        LinearPmDecision::BlockOversizedTicket => "block-oversized-ticket",
        LinearPmDecision::BlockMissingMilestone => "block-missing-milestone",
        LinearPmDecision::BlockHigherPriorityAvailable => "block-higher-priority-available",
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

fn expected_oversized_ticket(state: &LinearPmState) -> bool {
    state.issue_fetched
        && !state.blocked_ticket
        && state.estimate_present
        && state.estimate_points >= OVERSIZED_POINTS
}

fn expected_missing_milestone(state: &LinearPmState) -> bool {
    state.issue_fetched
        && !state.blocked_ticket
        && !state.oversized_ticket
        && state.project_has_milestones
        && !state.milestone_present
}

fn expected_decision(state: &LinearPmState) -> LinearPmDecision {
    if !state.start_transition {
        LinearPmDecision::Allow
    } else if !state.issue_key_present {
        LinearPmDecision::BlockMissingIssueIdentifier
    } else if !state.issue_fetched {
        LinearPmDecision::BlockLiveAuthorityUnavailable
    } else if state.blocked_ticket {
        LinearPmDecision::BlockBlockedTicket
    } else if state.oversized_ticket {
        LinearPmDecision::BlockOversizedTicket
    } else if state.missing_milestone {
        LinearPmDecision::BlockMissingMilestone
    } else if state.higher_priority_available {
        LinearPmDecision::BlockHigherPriorityAvailable
    } else {
        LinearPmDecision::Allow
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "linear_pm")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn linear_pm_state_schema() -> StateSchema<LinearPmState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "target_tool",
                "tool_input_present",
                "start_transition",
                "issue_key_present",
                "issue_key_sha256",
                "issue_fetched",
                "live_authority_error_present",
                "live_authority_error_sha256",
                "ticket_identifier_present",
                "ticket_identifier_sha256",
                "blocked_ticket",
                "blocked_reason_present",
                "blocked_reason_sha256",
                "estimate_present",
                "estimate_points",
                "oversized_ticket",
                "project_has_milestones",
                "milestone_present",
                "missing_milestone",
                "target_priority_present",
                "target_priority",
                "target_assignee_present",
                "higher_priority_available",
                "higher_priority_ticket_present",
                "higher_priority_ticket_sha256",
                "blocking_finding_count",
                "should_block",
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
                "target_tool": { "type": "boolean" },
                "tool_input_present": { "type": "boolean" },
                "start_transition": { "type": "boolean" },
                "issue_key_present": { "type": "boolean" },
                "issue_key_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "issue_fetched": { "type": "boolean" },
                "live_authority_error_present": { "type": "boolean" },
                "live_authority_error_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "ticket_identifier_present": { "type": "boolean" },
                "ticket_identifier_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "blocked_ticket": { "type": "boolean" },
                "blocked_reason_present": { "type": "boolean" },
                "blocked_reason_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "estimate_present": { "type": "boolean" },
                "estimate_points": { "type": "number", "minimum": 0 },
                "oversized_ticket": { "type": "boolean" },
                "project_has_milestones": { "type": "boolean" },
                "milestone_present": { "type": "boolean" },
                "missing_milestone": { "type": "boolean" },
                "target_priority_present": { "type": "boolean" },
                "target_priority": { "type": "integer", "minimum": 0, "maximum": 4 },
                "target_assignee_present": { "type": "boolean" },
                "higher_priority_available": { "type": "boolean" },
                "higher_priority_ticket_present": { "type": "boolean" },
                "higher_priority_ticket_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "BlockMissingIssueIdentifier",
                        "BlockLiveAuthorityUnavailable",
                        "BlockBlockedTicket",
                        "BlockOversizedTicket",
                        "BlockMissingMilestone",
                        "BlockHigherPriorityAvailable"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "linear_pm",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &LinearPmState| {
            let tool = state
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .ok_or_else(|| {
                    StateError::ValidationFailed(
                        "LangGraph tool-authority state requires concrete tool identity"
                            .to_string(),
                    )
                })?;
            if state.target_tool && tool != TARGET_TOOL {
                return Err(StateError::ValidationFailed(format!(
                    "linear_pm target_tool requires {TARGET_TOOL}, got {}",
                    tool
                )));
            }
            if !state.target_tool {
                if state.tool_input_present
                    || state.start_transition
                    || state.issue_key_present
                    || state.issue_key_sha256.is_some()
                    || state.issue_fetched
                    || state.live_authority_error_present
                    || state.live_authority_error_sha256.is_some()
                    || state.ticket_identifier_present
                    || state.ticket_identifier_sha256.is_some()
                    || state.blocked_ticket
                    || state.blocked_reason_present
                    || state.blocked_reason_sha256.is_some()
                    || state.estimate_present
                    || state.estimate_points != 0.0
                    || state.oversized_ticket
                    || state.project_has_milestones
                    || state.milestone_present
                    || state.missing_milestone
                    || state.target_priority_present
                    || state.target_priority != 0
                    || state.target_assignee_present
                    || state.higher_priority_available
                    || state.higher_priority_ticket_present
                    || state.higher_priority_ticket_sha256.is_some()
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "linear_pm non-target tool cannot carry PM authority facts".to_string(),
                    ));
                }
            }

            if state.start_transition && (!state.target_tool || !state.tool_input_present) {
                return Err(StateError::ValidationFailed(
                    "linear_pm start transition requires target tool input".to_string(),
                ));
            }
            if !state.start_transition {
                if state.issue_key_present
                    || state.issue_key_sha256.is_some()
                    || state.issue_fetched
                    || state.live_authority_error_present
                    || state.live_authority_error_sha256.is_some()
                    || state.ticket_identifier_present
                    || state.ticket_identifier_sha256.is_some()
                    || state.blocked_ticket
                    || state.blocked_reason_present
                    || state.blocked_reason_sha256.is_some()
                    || state.estimate_present
                    || state.estimate_points != 0.0
                    || state.oversized_ticket
                    || state.project_has_milestones
                    || state.milestone_present
                    || state.missing_milestone
                    || state.target_priority_present
                    || state.target_priority != 0
                    || state.target_assignee_present
                    || state.higher_priority_available
                    || state.higher_priority_ticket_present
                    || state.higher_priority_ticket_sha256.is_some()
                    || state.blocking_finding_count > 0
                    || state.should_block
                {
                    return Err(StateError::ValidationFailed(
                        "linear_pm non-start transition cannot carry PM authority facts"
                            .to_string(),
                    ));
                }
            }

            if state.issue_key_present {
                if !optional_hex_digest_present(&state.issue_key_sha256) {
                    return Err(StateError::ValidationFailed(
                        "linear_pm issue_key_sha256 must be a 64-character hex digest".to_string(),
                    ));
                }
            } else if state.issue_key_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "linear_pm missing issue key cannot carry an issue hash".to_string(),
                ));
            }

            if state.live_authority_error_present {
                if !state.start_transition || !state.issue_key_present || state.issue_fetched {
                    return Err(StateError::ValidationFailed(
                        "linear_pm live authority error only applies after failed live lookup"
                            .to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.live_authority_error_sha256) {
                    return Err(StateError::ValidationFailed(
                        "linear_pm live_authority_error_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.live_authority_error_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "linear_pm missing live authority error cannot carry an error hash"
                        .to_string(),
                ));
            }

            if state.issue_fetched {
                if !state.issue_key_present || state.live_authority_error_present {
                    return Err(StateError::ValidationFailed(
                        "linear_pm fetched issue requires issue key and no live error".to_string(),
                    ));
                }
                if !state.ticket_identifier_present
                    || !optional_hex_digest_present(&state.ticket_identifier_sha256)
                {
                    return Err(StateError::ValidationFailed(
                        "linear_pm fetched issue requires ticket identifier hash".to_string(),
                    ));
                }
            } else if state.ticket_identifier_present
                || state.ticket_identifier_sha256.is_some()
                || state.blocked_ticket
                || state.blocked_reason_present
                || state.blocked_reason_sha256.is_some()
                || state.estimate_present
                || state.estimate_points != 0.0
                || state.oversized_ticket
                || state.project_has_milestones
                || state.milestone_present
                || state.missing_milestone
                || state.target_priority_present
                || state.target_priority != 0
                || state.target_assignee_present
                || state.higher_priority_available
                || state.higher_priority_ticket_present
                || state.higher_priority_ticket_sha256.is_some()
            {
                return Err(StateError::ValidationFailed(
                    "linear_pm missing fetched issue cannot carry issue-derived facts".to_string(),
                ));
            }

            if state.blocked_ticket {
                if !state.issue_fetched || !state.blocked_reason_present {
                    return Err(StateError::ValidationFailed(
                        "linear_pm blocked ticket requires fetched issue and blocked reason"
                            .to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.blocked_reason_sha256) {
                    return Err(StateError::ValidationFailed(
                        "linear_pm blocked_reason_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
                if state.estimate_present
                    || state.estimate_points != 0.0
                    || state.oversized_ticket
                    || state.project_has_milestones
                    || state.milestone_present
                    || state.missing_milestone
                    || state.target_priority_present
                    || state.target_priority != 0
                    || state.target_assignee_present
                    || state.higher_priority_available
                    || state.higher_priority_ticket_present
                    || state.higher_priority_ticket_sha256.is_some()
                {
                    return Err(StateError::ValidationFailed(
                        "linear_pm blocked-ticket denial must not carry later PM facts".to_string(),
                    ));
                }
            } else if state.blocked_reason_present || state.blocked_reason_sha256.is_some() {
                return Err(StateError::ValidationFailed(
                    "linear_pm unblocked ticket cannot carry a blocked reason".to_string(),
                ));
            }

            if state.estimate_present {
                if !state.issue_fetched || !state.estimate_points.is_finite() || state.estimate_points <= 0.0 {
                    return Err(StateError::ValidationFailed(
                        "linear_pm estimate requires fetched issue and positive finite points"
                            .to_string(),
                    ));
                }
            } else if state.estimate_points != 0.0 || state.oversized_ticket {
                return Err(StateError::ValidationFailed(
                    "linear_pm missing estimate cannot carry estimate/oversized facts".to_string(),
                ));
            }

            let expected_oversized_ticket = expected_oversized_ticket(state);
            if state.oversized_ticket != expected_oversized_ticket {
                return Err(StateError::ValidationFailed(format!(
                    "linear_pm oversized_ticket must match estimate policy: expected \
                     {expected_oversized_ticket}, got {}",
                    state.oversized_ticket
                )));
            }

            if state.oversized_ticket
                && (state.project_has_milestones
                    || state.milestone_present
                    || state.missing_milestone
                    || state.target_priority_present
                    || state.target_priority != 0
                    || state.target_assignee_present
                    || state.higher_priority_available
                    || state.higher_priority_ticket_present
                    || state.higher_priority_ticket_sha256.is_some())
            {
                return Err(StateError::ValidationFailed(
                    "linear_pm oversized-ticket denial must not carry later PM facts".to_string(),
                ));
            }

            let expected_missing_milestone = expected_missing_milestone(state);
            if state.missing_milestone != expected_missing_milestone {
                return Err(StateError::ValidationFailed(format!(
                    "linear_pm missing_milestone must match milestone policy: expected \
                     {expected_missing_milestone}, got {}",
                    state.missing_milestone
                )));
            }

            if state.missing_milestone
                && (state.target_priority_present
                    || state.target_priority != 0
                    || state.target_assignee_present
                    || state.higher_priority_available
                    || state.higher_priority_ticket_present
                    || state.higher_priority_ticket_sha256.is_some())
            {
                return Err(StateError::ValidationFailed(
                    "linear_pm missing-milestone denial must not carry priority facts".to_string(),
                ));
            }

            if state.target_priority_present {
                if !state.issue_fetched || !(1..=4).contains(&state.target_priority) {
                    return Err(StateError::ValidationFailed(
                        "linear_pm target priority must be 1..=4 on a fetched issue".to_string(),
                    ));
                }
            } else if state.target_priority != 0 {
                return Err(StateError::ValidationFailed(
                    "linear_pm absent target priority must carry priority value 0".to_string(),
                ));
            }

            if state.higher_priority_available {
                if !state.issue_fetched
                    || !state.target_priority_present
                    || !state.target_assignee_present
                    || !state.higher_priority_ticket_present
                    || !optional_hex_digest_present(&state.higher_priority_ticket_sha256)
                {
                    return Err(StateError::ValidationFailed(
                        "linear_pm higher priority finding requires target priority, assignee, and ticket hash"
                            .to_string(),
                    ));
                }
            } else if state.higher_priority_ticket_present
                || state.higher_priority_ticket_sha256.is_some()
            {
                return Err(StateError::ValidationFailed(
                    "linear_pm no higher-priority finding cannot carry higher-priority ticket"
                        .to_string(),
                ));
            }

            let expected_should_block = expected_decision(state) != LinearPmDecision::Allow;
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "linear_pm should_block must match derived PM policy: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }

            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "linear_pm blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }

            if state.decision != LinearPmDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "linear_pm terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }

            Ok(())
        })
}

fn classify_node(state: &LinearPmState) -> LinearPmState {
    let mut next = state.clone();
    next.decision = expected_decision(&next);
    next
}

pub async fn build_linear_pm_graph() -> Result<LinearPmGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("linear_pm").await?;
    build_linear_pm_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_linear_pm_graph_with_ephemeral_sqlite() -> Result<LinearPmGraph, String> {
    build_linear_pm_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_linear_pm_graph_with_database_path(db_path: &str) -> Result<LinearPmGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_linear_pm_graph_with_checkpointer(checkpointer).await
}

async fn build_linear_pm_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<LinearPmGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = linear_pm_state_schema();
    let builder = StateGraphBuilder::<LinearPmState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_node_with_config(
            CLASSIFY,
            |s: &LinearPmState| {
                emit_decision_node_event("linear_pm", CLASSIFY, &s.identifier)?;
                Ok(classify_node(s))
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_node_with_config(
            ALLOW,
            |s: &LinearPmState| {
                emit_decision_node_event("linear_pm", ALLOW, &s.identifier)?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_node_with_config(
            BLOCK_MISSING_ISSUE_IDENTIFIER,
            |s: &LinearPmState| {
                emit_decision_node_event(
                    "linear_pm",
                    BLOCK_MISSING_ISSUE_IDENTIFIER,
                    &s.identifier,
                )?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::BlockMissingIssueIdentifier;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_MISSING_ISSUE_IDENTIFIER,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_node_with_config(
            BLOCK_LIVE_AUTHORITY_UNAVAILABLE,
            |s: &LinearPmState| {
                emit_decision_node_event(
                    "linear_pm",
                    BLOCK_LIVE_AUTHORITY_UNAVAILABLE,
                    &s.identifier,
                )?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::BlockLiveAuthorityUnavailable;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_LIVE_AUTHORITY_UNAVAILABLE,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_node_with_config(
            BLOCK_BLOCKED_TICKET,
            |s: &LinearPmState| {
                emit_decision_node_event("linear_pm", BLOCK_BLOCKED_TICKET, &s.identifier)?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::BlockBlockedTicket;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_BLOCKED_TICKET,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_node_with_config(
            BLOCK_OVERSIZED_TICKET,
            |s: &LinearPmState| {
                emit_decision_node_event("linear_pm", BLOCK_OVERSIZED_TICKET, &s.identifier)?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::BlockOversizedTicket;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_OVERSIZED_TICKET,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_node_with_config(
            BLOCK_MISSING_MILESTONE,
            |s: &LinearPmState| {
                emit_decision_node_event("linear_pm", BLOCK_MISSING_MILESTONE, &s.identifier)?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::BlockMissingMilestone;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_MISSING_MILESTONE,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_node_with_config(
            BLOCK_HIGHER_PRIORITY_AVAILABLE,
            |s: &LinearPmState| {
                emit_decision_node_event(
                    "linear_pm",
                    BLOCK_HIGHER_PRIORITY_AVAILABLE,
                    &s.identifier,
                )?;
                let mut next = s.clone();
                next.decision = LinearPmDecision::BlockHigherPriorityAvailable;
                Ok::<_, NodeError>(next)
            },
            node_config(
                BLOCK_HIGHER_PRIORITY_AVAILABLE,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &LinearPmState| match expected_decision(s) {
            LinearPmDecision::Allow => ALLOW.into(),
            LinearPmDecision::BlockMissingIssueIdentifier => BLOCK_MISSING_ISSUE_IDENTIFIER.into(),
            LinearPmDecision::BlockLiveAuthorityUnavailable => {
                BLOCK_LIVE_AUTHORITY_UNAVAILABLE.into()
            }
            LinearPmDecision::BlockBlockedTicket => BLOCK_BLOCKED_TICKET.into(),
            LinearPmDecision::BlockOversizedTicket => BLOCK_OVERSIZED_TICKET.into(),
            LinearPmDecision::BlockMissingMilestone => BLOCK_MISSING_MILESTONE.into(),
            LinearPmDecision::BlockHigherPriorityAvailable => {
                BLOCK_HIGHER_PRIORITY_AVAILABLE.into()
            }
            LinearPmDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(BLOCK_MISSING_ISSUE_IDENTIFIER, END)
        .add_edge(BLOCK_LIVE_AUTHORITY_UNAVAILABLE, END)
        .add_edge(BLOCK_BLOCKED_TICKET, END)
        .add_edge(BLOCK_OVERSIZED_TICKET, END)
        .add_edge(BLOCK_MISSING_MILESTONE, END)
        .add_edge(BLOCK_HIGHER_PRIORITY_AVAILABLE, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_linear_pm_decision_report(
    compiled: &LinearPmGraph,
    state: LinearPmState,
) -> Result<LinearPmGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "linear_pm",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "linear_pm", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "linear_pm",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(LinearPmGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: linear_pm_graph_topology(compiled)?,
    })
}

pub fn linear_pm_graph_topology(compiled: &LinearPmGraph) -> Result<DecisionGraphTopology, String> {
    topology("linear_pm", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::linear_pm_gate::{
        LinearPmDecision as AppDecision, LinearPmEvaluation,
    };

    fn allow_evaluation() -> LinearPmEvaluation {
        LinearPmEvaluation {
            tool: Some(TARGET_TOOL.to_string()),
            target_tool: true,
            tool_input_present: true,
            start_transition: true,
            issue_key: Some("FPCRM-1".to_string()),
            issue_key_present: true,
            issue_fetched: true,
            live_authority_error: None,
            ticket_identifier: Some("FPCRM-1".to_string()),
            blocked_ticket: false,
            blocked_reason: None,
            estimate_present: true,
            estimate_points: 3.0,
            oversized_ticket: false,
            project_has_milestones: true,
            milestone_present: true,
            missing_milestone: false,
            target_priority_present: true,
            target_priority: 2,
            target_assignee_present: true,
            higher_priority_available: false,
            higher_priority_ticket: None,
            should_block: false,
            decision: AppDecision::Allow,
        }
    }

    fn live_authority_block_evaluation() -> LinearPmEvaluation {
        LinearPmEvaluation {
            issue_fetched: false,
            live_authority_error: Some("Linear transport error: timeout".to_string()),
            ticket_identifier: None,
            estimate_present: false,
            estimate_points: 0.0,
            project_has_milestones: false,
            milestone_present: false,
            target_priority_present: false,
            target_priority: 0,
            target_assignee_present: false,
            should_block: true,
            decision: AppDecision::BlockLiveAuthorityUnavailable,
            ..allow_evaluation()
        }
    }

    fn oversized_block_evaluation() -> LinearPmEvaluation {
        LinearPmEvaluation {
            estimate_points: 8.0,
            oversized_ticket: true,
            project_has_milestones: false,
            milestone_present: false,
            target_priority_present: false,
            target_priority: 0,
            target_assignee_present: false,
            should_block: true,
            decision: AppDecision::BlockOversizedTicket,
            ..allow_evaluation()
        }
    }

    #[tokio::test]
    async fn graph_authorizes_clean_allow() {
        let graph = build_linear_pm_graph_with_ephemeral_sqlite().await.unwrap();
        let state = LinearPmState::from_evaluation("linear-pm-allow", &allow_evaluation());
        let run = run_linear_pm_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, LinearPmDecision::Allow);
        assert!(run
            .linear_pm_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let graph = build_linear_pm_graph_with_ephemeral_sqlite().await.unwrap();
        let state = LinearPmState::from_evaluation("linear-pm-allow", &allow_evaluation());
        let mut run = run_linear_pm_decision_report(&graph, state).await.unwrap();
        run.write_history.clear();

        let err = run
            .linear_pm_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_authorizes_live_authority_block() {
        let graph = build_linear_pm_graph_with_ephemeral_sqlite().await.unwrap();
        let state = LinearPmState::from_evaluation(
            "linear-pm-live-authority",
            &live_authority_block_evaluation(),
        );
        let run = run_linear_pm_decision_report(&graph, state).await.unwrap();
        assert_eq!(
            run.state.decision,
            LinearPmDecision::BlockLiveAuthorityUnavailable
        );
    }

    #[tokio::test]
    async fn graph_authorizes_oversized_block() {
        let graph = build_linear_pm_graph_with_ephemeral_sqlite().await.unwrap();
        let state =
            LinearPmState::from_evaluation("linear-pm-oversized", &oversized_block_evaluation());
        let run = run_linear_pm_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, LinearPmDecision::BlockOversizedTicket);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_oversized_allow() {
        let graph = build_linear_pm_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state =
            LinearPmState::from_evaluation("linear-pm-forged", &oversized_block_evaluation());
        state.oversized_ticket = false;
        state.blocking_finding_count = 0;
        state.should_block = false;
        let err = run_linear_pm_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("oversized_ticket"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_out_of_order_block_facts() {
        let graph = build_linear_pm_graph_with_ephemeral_sqlite().await.unwrap();
        let mut state =
            LinearPmState::from_evaluation("linear-pm-out-of-order", &oversized_block_evaluation());
        state.blocked_ticket = true;
        state.blocked_reason_present = true;
        state.blocked_reason_sha256 = Some(sha256("blocked by FPCRM-2"));
        let err = run_linear_pm_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("blocked-ticket"), "{err}");
    }
}
