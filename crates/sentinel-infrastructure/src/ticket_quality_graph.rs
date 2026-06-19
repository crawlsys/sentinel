//! Graph-backed Linear ticket quality authorization.
//!
//! The application hook computes deterministic facts for Linear create/update
//! calls: whether the tool is in scope, whether the JSON input is inspectable,
//! which Definition-of-Ready dimensions are missing, and the derived allow/deny
//! decision. This graph authorizes those facts through durable LangGraph
//! checkpoints before the CLI permits a Linear ticket write.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::ticket_quality_gate::TicketQualityEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TicketQualityDecision {
    #[default]
    Unclassified,
    Allow,
    DenyMalformedInput,
    DenyMissingFields,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketQualityState {
    pub identifier: String,
    pub tool: Option<String>,
    pub ticket_write_tool: bool,
    pub create_issue: bool,
    pub update_issue: bool,
    pub tool_input_present: bool,
    pub tool_input_object: bool,
    pub tool_input_sha256: Option<String>,
    pub malformed_input: bool,
    pub missing_field_count: u64,
    pub missing_estimate: bool,
    pub missing_priority: bool,
    pub missing_label_ids: bool,
    pub missing_description: bool,
    pub blocking_finding_count: u64,
    pub should_deny: bool,
    pub decision: TicketQualityDecision,
}

impl TicketQualityState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &TicketQualityEvaluation,
    ) -> Self {
        let malformed_input = matches!(
            evaluation.decision,
            sentinel_application::hooks::ticket_quality_gate::TicketQualityDecision::DenyMalformedInput
        );
        let missing_field_count = evaluation.missing.len() as u64;
        let blocking_finding_count = u64::from(expected_should_deny(
            evaluation.ticket_write_tool,
            malformed_input,
            missing_field_count,
        ));
        Self {
            identifier: identifier.into(),
            tool: evaluation
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|tool| !tool.is_empty())
                .map(ToString::to_string),
            ticket_write_tool: evaluation.ticket_write_tool,
            create_issue: evaluation.create_issue,
            update_issue: evaluation.update_issue,
            tool_input_present: evaluation.tool_input_present,
            tool_input_object: evaluation.tool_input_object,
            tool_input_sha256: evaluation
                .tool_input_sha256
                .as_deref()
                .map(str::trim)
                .filter(|digest| !digest.is_empty() && evaluation.tool_input_present)
                .map(ToString::to_string),
            malformed_input,
            missing_field_count,
            missing_estimate: evaluation.missing_estimate,
            missing_priority: evaluation.missing_priority,
            missing_label_ids: evaluation.missing_label_ids,
            missing_description: evaluation.missing_description,
            blocking_finding_count,
            should_deny: blocking_finding_count > 0,
            decision: TicketQualityDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TicketQualityGraphRun {
    pub state: TicketQualityState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<TicketQualityState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct TicketQualityAuthorization {
    decision: TicketQualityDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl TicketQualityAuthorization {
    #[must_use]
    pub fn decision(&self) -> TicketQualityDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl TicketQualityGraphRun {
    #[must_use]
    pub fn ticket_quality_authorization(
        &self,
    ) -> Result<Option<TicketQualityAuthorization>, String> {
        if self.state.decision == TicketQualityDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "ticket_quality",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(TicketQualityAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const DENY_MALFORMED_INPUT: &str = "deny_malformed_input";
const DENY_MISSING_FIELDS: &str = "deny_missing_fields";

pub type TicketQualityGraph = CompilationResult<TicketQualityState>;

#[must_use]
pub fn ticket_quality_decision_label(decision: TicketQualityDecision) -> &'static str {
    match decision {
        TicketQualityDecision::Unclassified => "unclassified",
        TicketQualityDecision::Allow => "allow",
        TicketQualityDecision::DenyMalformedInput => "deny-malformed-input",
        TicketQualityDecision::DenyMissingFields => "deny-missing-fields",
    }
}

fn expected_should_deny(
    ticket_write_tool: bool,
    malformed_input: bool,
    missing_field_count: u64,
) -> bool {
    ticket_write_tool && (malformed_input || missing_field_count > 0)
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn missing_flag_count(state: &TicketQualityState) -> u64 {
    u64::from(state.missing_estimate)
        + u64::from(state.missing_priority)
        + u64::from(state.missing_label_ids)
        + u64::from(state.missing_description)
}

fn expected_decision(state: &TicketQualityState) -> TicketQualityDecision {
    if !state.ticket_write_tool {
        TicketQualityDecision::Allow
    } else if state.malformed_input {
        TicketQualityDecision::DenyMalformedInput
    } else if state.missing_field_count > 0 {
        TicketQualityDecision::DenyMissingFields
    } else {
        TicketQualityDecision::Allow
    }
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "ticket_quality")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn ticket_quality_state_schema() -> StateSchema<TicketQualityState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "ticket_write_tool",
                "create_issue",
                "update_issue",
                "tool_input_present",
                "tool_input_object",
                "tool_input_sha256",
                "malformed_input",
                "missing_field_count",
                "missing_estimate",
                "missing_priority",
                "missing_label_ids",
                "missing_description",
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
                "ticket_write_tool": { "type": "boolean" },
                "create_issue": { "type": "boolean" },
                "update_issue": { "type": "boolean" },
                "tool_input_present": { "type": "boolean" },
                "tool_input_object": { "type": "boolean" },
                "tool_input_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "malformed_input": { "type": "boolean" },
                "missing_field_count": { "type": "integer", "minimum": 0, "maximum": 4 },
                "missing_estimate": { "type": "boolean" },
                "missing_priority": { "type": "boolean" },
                "missing_label_ids": { "type": "boolean" },
                "missing_description": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_deny": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "DenyMalformedInput",
                        "DenyMissingFields"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "ticket_quality",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &TicketQualityState| {
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
            if !state.ticket_write_tool {
                if state.create_issue
                    || state.update_issue
                    || state.tool_input_present
                    || state.tool_input_object
                    || state.tool_input_sha256.is_some()
                    || state.malformed_input
                    || state.missing_field_count > 0
                    || missing_flag_count(state) > 0
                    || state.blocking_finding_count > 0
                    || state.should_deny
                {
                    return Err(StateError::ValidationFailed(
                        "ticket_quality non-ticket state cannot carry ticket-write facts"
                            .to_string(),
                    ));
                }
            } else {
                if state.create_issue == state.update_issue {
                    return Err(StateError::ValidationFailed(
                        "ticket_quality ticket write must be exactly one of create/update"
                            .to_string(),
                    ));
                }
                if tool != "mcp__linear__create_issue" && tool != "mcp__linear__update_issue"
                {
                    return Err(StateError::ValidationFailed(format!(
                        "ticket_quality ticket_write_tool requires a Linear ticket write tool, got {}",
                        tool
                    )));
                }
            }
            if state.tool_input_present {
                if !optional_hex_digest_present(&state.tool_input_sha256) {
                    return Err(StateError::ValidationFailed(
                        "ticket_quality tool_input_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.tool_input_sha256.is_some() || state.tool_input_object {
                return Err(StateError::ValidationFailed(
                    "ticket_quality missing tool_input cannot carry input hash/object facts"
                        .to_string(),
                ));
            }
            let expected_malformed_input =
                state.ticket_write_tool && (!state.tool_input_present || !state.tool_input_object);
            if state.malformed_input != expected_malformed_input {
                return Err(StateError::ValidationFailed(format!(
                    "ticket_quality malformed_input must match inspectability: expected \
                     {expected_malformed_input}, got {}",
                    state.malformed_input
                )));
            }
            if state.malformed_input {
                if state.missing_field_count > 0 || missing_flag_count(state) > 0 {
                    return Err(StateError::ValidationFailed(
                        "ticket_quality malformed input cannot also carry readiness fields"
                            .to_string(),
                    ));
                }
            } else if state.ticket_write_tool {
                if !state.tool_input_present || !state.tool_input_object {
                    return Err(StateError::ValidationFailed(
                        "ticket_quality inspectable ticket write requires object input"
                            .to_string(),
                    ));
                }
                let expected_missing_field_count = missing_flag_count(state);
                if state.missing_field_count != expected_missing_field_count {
                    return Err(StateError::ValidationFailed(format!(
                        "ticket_quality missing_field_count must match field flags: expected \
                         {expected_missing_field_count}, got {}",
                        state.missing_field_count
                    )));
                }
            }
            let expected_should_deny = expected_should_deny(
                state.ticket_write_tool,
                state.malformed_input,
                state.missing_field_count,
            );
            if state.should_deny != expected_should_deny {
                return Err(StateError::ValidationFailed(format!(
                    "ticket_quality should_deny must match ticket quality policy: expected \
                     {expected_should_deny}, got {}",
                    state.should_deny
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_deny);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "ticket_quality blocking_finding_count must match should_deny: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != TicketQualityDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "ticket_quality terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: TicketQualityState) -> Result<TicketQualityState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_ticket_quality_graph() -> Result<TicketQualityGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("ticket_quality").await?;
    build_ticket_quality_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_ticket_quality_graph_with_ephemeral_sqlite() -> Result<TicketQualityGraph, String> {
    build_ticket_quality_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_ticket_quality_graph_with_database_path(
    db_path: &str,
) -> Result<TicketQualityGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_ticket_quality_graph_with_checkpointer(checkpointer).await
}

async fn build_ticket_quality_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<TicketQualityGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = ticket_quality_state_schema();
    let builder = StateGraphBuilder::<TicketQualityState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: TicketQualityState| async move {
                emit_decision_node_event("ticket_quality", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ALLOW,
            |s: TicketQualityState| async move {
                emit_decision_node_event("ticket_quality", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = TicketQualityDecision::Allow;
                Ok::<_, NodeError>(next)
            },
            node_config(ALLOW, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            DENY_MALFORMED_INPUT,
            |s: TicketQualityState| async move {
                emit_decision_node_event("ticket_quality", DENY_MALFORMED_INPUT, &s.identifier)?;
                let mut next = s;
                next.decision = TicketQualityDecision::DenyMalformedInput;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_MALFORMED_INPUT,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_async_node_with_config(
            DENY_MISSING_FIELDS,
            |s: TicketQualityState| async move {
                emit_decision_node_event("ticket_quality", DENY_MISSING_FIELDS, &s.identifier)?;
                let mut next = s;
                next.decision = TicketQualityDecision::DenyMissingFields;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY_MISSING_FIELDS,
                checkpointer_backend,
                checkpointer_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &TicketQualityState| {
            match expected_decision(s) {
                TicketQualityDecision::Allow => ALLOW.into(),
                TicketQualityDecision::DenyMalformedInput => DENY_MALFORMED_INPUT.into(),
                TicketQualityDecision::DenyMissingFields => DENY_MISSING_FIELDS.into(),
                TicketQualityDecision::Unclassified => ALLOW.into(),
            }
        })
        .add_edge(ALLOW, END)
        .add_edge(DENY_MALFORMED_INPUT, END)
        .add_edge(DENY_MISSING_FIELDS, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_ticket_quality_decision_report(
    compiled: &TicketQualityGraph,
    state: TicketQualityState,
) -> Result<TicketQualityGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "ticket_quality",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "ticket_quality", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "ticket_quality",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(TicketQualityGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: ticket_quality_graph_topology(compiled)?,
    })
}

pub fn ticket_quality_graph_topology(
    compiled: &TicketQualityGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("ticket_quality", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::ticket_quality_gate::{
        TicketQualityDecision as AppDecision, TicketQualityEvaluation,
    };

    fn evaluation(decision: AppDecision) -> TicketQualityEvaluation {
        TicketQualityEvaluation {
            tool: Some("mcp__linear__create_issue".to_string()),
            ticket_write_tool: true,
            create_issue: true,
            update_issue: false,
            tool_input_present: true,
            tool_input_object: true,
            tool_input_sha256: Some(crate::agent_revocation_graph::sha256("input")),
            missing: Vec::new(),
            malformed_reason: None,
            missing_estimate: false,
            missing_priority: false,
            missing_label_ids: false,
            missing_description: false,
            should_deny: false,
            decision,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_ready_ticket_allow() {
        let graph = build_ticket_quality_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = TicketQualityState::from_evaluation(
            "ticket-quality-allow",
            &evaluation(AppDecision::Allow),
        );
        let run = run_ticket_quality_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TicketQualityDecision::Allow);
        assert!(run
            .ticket_quality_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_missing_fields_deny() {
        let graph = build_ticket_quality_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(AppDecision::DenyMissingFields);
        eval.missing_estimate = true;
        eval.missing_priority = true;
        eval.missing = vec![
            sentinel_application::hooks::ticket_quality_gate::Missing {
                field: "estimate",
                why: "no estimate set",
            },
            sentinel_application::hooks::ticket_quality_gate::Missing {
                field: "priority",
                why: "no priority set",
            },
        ];
        eval.should_deny = true;
        let state = TicketQualityState::from_evaluation("ticket-quality-missing", &eval);
        let run = run_ticket_quality_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, TicketQualityDecision::DenyMissingFields);
    }

    #[tokio::test]
    async fn graph_authorizes_malformed_input_deny() {
        let graph = build_ticket_quality_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(AppDecision::DenyMalformedInput);
        eval.tool_input_present = false;
        eval.tool_input_object = false;
        eval.tool_input_sha256 = None;
        eval.malformed_reason = Some("missing tool_input");
        eval.should_deny = true;
        let state = TicketQualityState::from_evaluation("ticket-quality-malformed", &eval);
        let run = run_ticket_quality_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            TicketQualityDecision::DenyMalformedInput
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_with_missing_fields() {
        let graph = build_ticket_quality_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(AppDecision::DenyMissingFields);
        eval.missing_estimate = true;
        eval.missing = vec![sentinel_application::hooks::ticket_quality_gate::Missing {
            field: "estimate",
            why: "no estimate set",
        }];
        eval.should_deny = true;
        let mut state = TicketQualityState::from_evaluation("ticket-quality-forged", &eval);
        state.should_deny = false;
        state.blocking_finding_count = 0;
        let err = run_ticket_quality_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_deny"), "{err}");
    }
}
