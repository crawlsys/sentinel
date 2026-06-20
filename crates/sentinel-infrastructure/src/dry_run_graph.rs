//! Graph-backed A3 dry-run authorization.
//!
//! The application hook computes deterministic A3 facts: reversibility scope,
//! approval-marker state, dry-run completeness, and the dual-auditor verdict.
//! This graph authorizes the resulting allow/block decision through durable
//! LangGraph checkpoints so irreversible actions do not commit from an
//! uncheckpointed branch.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::dry_run_then_commit::{
    DryRunGateEvaluation, HUMAN_SAMPLE_CONFIDENCE_THRESHOLD,
};
use sentinel_domain::reversibility::ReversibilityClass;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DryRunDecision {
    #[default]
    Unclassified,
    Allow,
    AllowAndRecordApproval,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunState {
    pub identifier: String,
    pub class: String,
    pub a3_scope: bool,
    pub readonly_mcp_allow: bool,
    pub bash_schema_allow: bool,
    pub catastrophic_shell_block: bool,
    pub session_present: bool,
    pub missing_session: bool,
    pub approval_present: bool,
    pub dry_run_required: bool,
    pub dry_run_complete: bool,
    pub auditor_required: bool,
    pub auditor_attempted: bool,
    pub auditor_error_count: u64,
    pub auditor_passed: bool,
    pub auditor_blocked: bool,
    pub auditor_confidence_millis: u64,
    pub threshold_millis: u64,
    pub low_confidence: bool,
    pub catastrophic_class: bool,
    pub human_review_required: bool,
    pub approval_marker_should_be_recorded: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: DryRunDecision,
}

impl DryRunState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &DryRunGateEvaluation,
    ) -> Self {
        let class = evaluation
            .class
            .map(reversibility_class_label)
            .unwrap_or("not_classified")
            .to_string();
        let catastrophic_class = matches!(evaluation.class, Some(ReversibilityClass::Catastrophic));
        let auditor_passed = evaluation.auditor_passed();
        let auditor_blocked = evaluation.auditor_blocked();
        let auditor_error_count = u64::from(evaluation.auditor_error.is_some());
        let auditor_confidence_millis = evaluation
            .auditor_verdict
            .as_ref()
            .map(|verdict| axis_to_millis(verdict.confidence))
            .unwrap_or(0);
        let threshold_millis = axis_to_millis(HUMAN_SAMPLE_CONFIDENCE_THRESHOLD);
        let session_present = evaluation.session_id.is_some();
        let dry_run_required = expected_dry_run_required(
            evaluation.a3_scope,
            evaluation.readonly_mcp_allow,
            evaluation.bash_schema_allow,
            evaluation.catastrophic_shell_block,
            evaluation.missing_session,
            evaluation.approval_present,
        );
        let auditor_required = dry_run_required && evaluation.dry_run_complete;
        let blocking_finding_count = expected_blocking_finding_count(
            evaluation.catastrophic_shell_block,
            evaluation.missing_session,
            dry_run_required && !evaluation.dry_run_complete,
            auditor_error_count,
            auditor_blocked,
            evaluation.human_review_required,
        );
        Self {
            identifier: identifier.into(),
            class,
            a3_scope: evaluation.a3_scope,
            readonly_mcp_allow: evaluation.readonly_mcp_allow,
            bash_schema_allow: evaluation.bash_schema_allow,
            catastrophic_shell_block: evaluation.catastrophic_shell_block,
            session_present,
            missing_session: evaluation.missing_session,
            approval_present: evaluation.approval_present,
            dry_run_required,
            dry_run_complete: evaluation.dry_run_complete,
            auditor_required,
            auditor_attempted: evaluation.auditor_attempted,
            auditor_error_count,
            auditor_passed,
            auditor_blocked,
            auditor_confidence_millis,
            threshold_millis,
            low_confidence: evaluation.low_confidence,
            catastrophic_class,
            human_review_required: evaluation.human_review_required,
            approval_marker_should_be_recorded: evaluation.approval_marker_should_be_recorded,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: DryRunDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DryRunGraphRun {
    pub state: DryRunState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DryRunState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct DryRunAuthorization {
    decision: DryRunDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl DryRunAuthorization {
    #[must_use]
    pub fn decision(&self) -> DryRunDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl DryRunGraphRun {
    #[must_use]
    pub fn dry_run_authorization(&self) -> Result<Option<DryRunAuthorization>, String> {
        if self.state.decision == DryRunDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "dry_run",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(DryRunAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const ALLOW_AND_RECORD_APPROVAL: &str = "allow_and_record_approval";
const BLOCK: &str = "block";

pub type DryRunGraph = CompilationResult<DryRunState>;

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
pub fn dry_run_decision_label(decision: DryRunDecision) -> &'static str {
    match decision {
        DryRunDecision::Unclassified => "unclassified",
        DryRunDecision::Allow => "allow",
        DryRunDecision::AllowAndRecordApproval => "allow-and-record-approval",
        DryRunDecision::Block => "block",
    }
}

fn axis_to_millis(axis: f32) -> u64 {
    (axis.clamp(0.0, 1.0) * 1000.0).round() as u64
}

fn expected_dry_run_required(
    a3_scope: bool,
    readonly_mcp_allow: bool,
    bash_schema_allow: bool,
    catastrophic_shell_block: bool,
    missing_session: bool,
    approval_present: bool,
) -> bool {
    a3_scope
        && !readonly_mcp_allow
        && !bash_schema_allow
        && !catastrophic_shell_block
        && !missing_session
        && !approval_present
}

fn expected_blocking_finding_count(
    catastrophic_shell_block: bool,
    missing_session: bool,
    incomplete_dry_run: bool,
    auditor_error_count: u64,
    auditor_blocked: bool,
    human_review_required: bool,
) -> u64 {
    u64::from(catastrophic_shell_block)
        + u64::from(missing_session)
        + u64::from(incomplete_dry_run)
        + auditor_error_count
        + u64::from(auditor_blocked)
        + u64::from(human_review_required)
}

fn expected_approval_marker_should_be_recorded(state: &DryRunState) -> bool {
    state.a3_scope
        && state.auditor_required
        && state.auditor_passed
        && !state.approval_present
        && !state.should_block
}

fn expected_decision(state: &DryRunState) -> DryRunDecision {
    if state.should_block {
        DryRunDecision::Block
    } else if state.approval_marker_should_be_recorded {
        DryRunDecision::AllowAndRecordApproval
    } else {
        DryRunDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "dry_run")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn dry_run_state_schema() -> StateSchema<DryRunState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "class",
                "a3_scope",
                "readonly_mcp_allow",
                "bash_schema_allow",
                "catastrophic_shell_block",
                "session_present",
                "missing_session",
                "approval_present",
                "dry_run_required",
                "dry_run_complete",
                "auditor_required",
                "auditor_attempted",
                "auditor_error_count",
                "auditor_passed",
                "auditor_blocked",
                "auditor_confidence_millis",
                "threshold_millis",
                "low_confidence",
                "catastrophic_class",
                "human_review_required",
                "approval_marker_should_be_recorded",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "class": {
                    "type": "string",
                    "enum": [
                        "not_classified",
                        "trivially_reversible",
                        "reversible_with_effort",
                        "irreversible",
                        "catastrophic"
                    ]
                },
                "a3_scope": { "type": "boolean" },
                "readonly_mcp_allow": { "type": "boolean" },
                "bash_schema_allow": { "type": "boolean" },
                "catastrophic_shell_block": { "type": "boolean" },
                "session_present": { "type": "boolean" },
                "missing_session": { "type": "boolean" },
                "approval_present": { "type": "boolean" },
                "dry_run_required": { "type": "boolean" },
                "dry_run_complete": { "type": "boolean" },
                "auditor_required": { "type": "boolean" },
                "auditor_attempted": { "type": "boolean" },
                "auditor_error_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "auditor_passed": { "type": "boolean" },
                "auditor_blocked": { "type": "boolean" },
                "auditor_confidence_millis": { "type": "integer", "minimum": 0, "maximum": 1000 },
                "threshold_millis": { "type": "integer", "minimum": 0, "maximum": 1000 },
                "low_confidence": { "type": "boolean" },
                "catastrophic_class": { "type": "boolean" },
                "human_review_required": { "type": "boolean" },
                "approval_marker_should_be_recorded": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "AllowAndRecordApproval", "Block"]
                }
            },
            "x-sentinel": {
                "graph": "dry_run",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &DryRunState| {
            let shortcut_count = u64::from(state.readonly_mcp_allow)
                + u64::from(state.bash_schema_allow)
                + u64::from(state.catastrophic_shell_block);
            if shortcut_count > 1 {
                return Err(StateError::ValidationFailed(
                    "dry_run shortcut decisions must be mutually exclusive".to_string(),
                ));
            }
            if state.a3_scope && state.class == "not_classified" {
                return Err(StateError::ValidationFailed(
                    "dry_run A3 scope requires a reversibility class".to_string(),
                ));
            }
            if !state.a3_scope
                && (state.readonly_mcp_allow
                    || state.bash_schema_allow
                    || state.catastrophic_shell_block
                    || state.missing_session
                    || state.approval_present
                    || state.dry_run_required
                    || state.auditor_required
                    || state.auditor_attempted
                    || state.auditor_error_count > 0
                    || state.auditor_passed
                    || state.auditor_blocked
                    || state.low_confidence
                    || state.human_review_required
                    || state.approval_marker_should_be_recorded
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "dry_run out-of-scope state cannot carry A3 authorization facts".to_string(),
                ));
            }
            if state.bash_schema_allow && state.catastrophic_class {
                return Err(StateError::ValidationFailed(
                    "dry_run catastrophic Bash must block instead of schema-allow".to_string(),
                ));
            }
            let expected_missing_session = state.a3_scope
                && !state.readonly_mcp_allow
                && !state.bash_schema_allow
                && !state.catastrophic_shell_block
                && !state.session_present;
            if state.missing_session != expected_missing_session {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run missing_session must match A3 scope and session presence: expected \
                     {expected_missing_session}, got {}",
                    state.missing_session
                )));
            }
            if state.approval_present && !state.session_present {
                return Err(StateError::ValidationFailed(
                    "dry_run approval_present requires session_present=true".to_string(),
                ));
            }
            let expected_dry_run_required = expected_dry_run_required(
                state.a3_scope,
                state.readonly_mcp_allow,
                state.bash_schema_allow,
                state.catastrophic_shell_block,
                state.missing_session,
                state.approval_present,
            );
            if state.dry_run_required != expected_dry_run_required {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run dry_run_required must match A3 preconditions: expected \
                     {expected_dry_run_required}, got {}",
                    state.dry_run_required
                )));
            }
            let expected_auditor_required = state.dry_run_required && state.dry_run_complete;
            if state.auditor_required != expected_auditor_required {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run auditor_required must match dry-run completeness: expected \
                     {expected_auditor_required}, got {}",
                    state.auditor_required
                )));
            }
            if state.auditor_attempted != state.auditor_required {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run auditor_attempted must equal auditor_required: expected {}, got {}",
                    state.auditor_required, state.auditor_attempted
                )));
            }
            if state.auditor_passed && state.auditor_blocked {
                return Err(StateError::ValidationFailed(
                    "dry_run auditor cannot both pass and block".to_string(),
                ));
            }
            if state.auditor_error_count > 0 && (state.auditor_passed || state.auditor_blocked) {
                return Err(StateError::ValidationFailed(
                    "dry_run auditor error cannot coexist with an auditor verdict".to_string(),
                ));
            }
            if state.auditor_required
                && state.auditor_error_count == 0
                && !state.auditor_passed
                && !state.auditor_blocked
            {
                return Err(StateError::ValidationFailed(
                    "dry_run auditor-required state must carry an auditor outcome".to_string(),
                ));
            }
            if !state.auditor_required
                && (state.auditor_error_count > 0
                    || state.auditor_passed
                    || state.auditor_blocked
                    || state.auditor_confidence_millis > 0)
            {
                return Err(StateError::ValidationFailed(
                    "dry_run auditor outcome cannot exist when auditor_required=false".to_string(),
                ));
            }
            let expected_low_confidence =
                state.auditor_passed && state.auditor_confidence_millis < state.threshold_millis;
            if state.low_confidence != expected_low_confidence {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run low_confidence must match auditor confidence threshold: expected \
                     {expected_low_confidence}, got {}",
                    state.low_confidence
                )));
            }
            let expected_human_review_required =
                state.auditor_passed && (state.low_confidence || state.catastrophic_class);
            if state.human_review_required != expected_human_review_required {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run human_review_required must match pass/confidence/class policy: \
                     expected {expected_human_review_required}, got {}",
                    state.human_review_required
                )));
            }
            let incomplete_dry_run = state.dry_run_required && !state.dry_run_complete;
            let expected_blocking_finding_count = expected_blocking_finding_count(
                state.catastrophic_shell_block,
                state.missing_session,
                incomplete_dry_run,
                state.auditor_error_count,
                state.auditor_blocked,
                state.human_review_required,
            );
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run blocking_finding_count must equal derived finding count: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            let expected_should_block = state.blocking_finding_count > 0;
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run should_block must match derived finding count: expected \
                     {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_record = expected_approval_marker_should_be_recorded(state);
            if state.approval_marker_should_be_recorded != expected_record {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run approval marker intent must match auditor authorization: expected \
                     {expected_record}, got {}",
                    state.approval_marker_should_be_recorded
                )));
            }
            if state.decision != DryRunDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "dry_run terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: DryRunState) -> Result<DryRunState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_dry_run_graph() -> Result<DryRunGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("dry_run").await?;
    build_dry_run_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_dry_run_graph_with_ephemeral_sqlite() -> Result<DryRunGraph, String> {
    build_dry_run_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_dry_run_graph_with_database_path(db_path: &str) -> Result<DryRunGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_dry_run_graph_with_checkpointer(checkpointer).await
}

async fn build_dry_run_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<DryRunGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = dry_run_state_schema();
    let builder = StateGraphBuilder::<DryRunState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: DryRunState| async move {
                emit_decision_node_event("dry_run", CLASSIFY, &s.identifier)?;
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
            |s: DryRunState| async move {
                emit_decision_node_event("dry_run", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = DryRunDecision::Allow;
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
            ALLOW_AND_RECORD_APPROVAL,
            |s: DryRunState| async move {
                emit_decision_node_event("dry_run", ALLOW_AND_RECORD_APPROVAL, &s.identifier)?;
                let mut next = s;
                next.decision = DryRunDecision::AllowAndRecordApproval;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_AND_RECORD_APPROVAL,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK,
            |s: DryRunState| async move {
                emit_decision_node_event("dry_run", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = DryRunDecision::Block;
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
        .add_conditional_edge(CLASSIFY, |s: &DryRunState| match expected_decision(s) {
            DryRunDecision::Allow => ALLOW.into(),
            DryRunDecision::AllowAndRecordApproval => ALLOW_AND_RECORD_APPROVAL.into(),
            DryRunDecision::Block => BLOCK.into(),
            DryRunDecision::Unclassified => ALLOW.into(),
        })
        .add_edge(ALLOW, END)
        .add_edge(ALLOW_AND_RECORD_APPROVAL, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_dry_run_decision_report(
    compiled: &DryRunGraph,
    state: DryRunState,
) -> Result<DryRunGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "dry_run",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(compiled, &thread_id, "dry_run", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "dry_run",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(DryRunGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: dry_run_graph_topology(compiled)?,
    })
}

pub fn dry_run_graph_topology(compiled: &DryRunGraph) -> Result<DecisionGraphTopology, String> {
    topology("dry_run", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::dry_run_then_commit::{
        DryRunGateDecision, DryRunGateEvaluation,
    };
    use sentinel_domain::dry_run::{AuditorAxes, AuditorDecision, AuditorVerdict};

    fn base_evaluation(class: ReversibilityClass) -> DryRunGateEvaluation {
        DryRunGateEvaluation {
            tool: Some("Edit".to_string()),
            class: Some(class),
            a3_scope: true,
            readonly_mcp_allow: false,
            bash_schema_allow: false,
            catastrophic_shell_block: false,
            session_id: Some("sess-1".to_string()),
            missing_session: false,
            action_hash: Some("abc123".to_string()),
            approval_present: false,
            dry_run_complete: true,
            auditor_attempted: true,
            auditor_error: None,
            auditor_verdict: None,
            low_confidence: false,
            human_review_required: false,
            should_block: false,
            approval_marker_should_be_recorded: false,
            decision: DryRunGateDecision::Allow,
            block_reason: None,
        }
    }

    fn pass_verdict(confidence: f32) -> AuditorVerdict {
        AuditorVerdict {
            decision: AuditorDecision::Pass,
            confidence,
            axes: AuditorAxes::new(0.9, 0.9, 0.9, 0.9),
            reasoning: "looks good".to_string(),
            auditor_model: "test:auditor".to_string(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_irreversible_pass_and_marker_record() {
        let graph = build_dry_run_graph_with_ephemeral_sqlite().await.unwrap();
        let mut evaluation = base_evaluation(ReversibilityClass::Irreversible);
        evaluation.auditor_verdict = Some(pass_verdict(0.95));
        evaluation.approval_marker_should_be_recorded = true;
        evaluation.decision = DryRunGateDecision::AllowAndRecordApproval;
        let state = DryRunState::from_evaluation("a3-pass", &evaluation);
        let run = run_dry_run_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, DryRunDecision::AllowAndRecordApproval);
        assert!(run
            .dry_run_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_incomplete_dry_run_block() {
        let graph = build_dry_run_graph_with_ephemeral_sqlite().await.unwrap();
        let mut evaluation = base_evaluation(ReversibilityClass::Irreversible);
        evaluation.dry_run_complete = false;
        evaluation.auditor_attempted = false;
        evaluation.should_block = true;
        evaluation.decision = DryRunGateDecision::Block;
        evaluation.block_reason = Some("missing dry-run fields".to_string());
        let state = DryRunState::from_evaluation("a3-incomplete", &evaluation);
        let run = run_dry_run_decision_report(&graph, state).await.unwrap();
        assert_eq!(run.state.decision, DryRunDecision::Block);
        assert!(run.state.should_block);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_marker_record() {
        let graph = build_dry_run_graph_with_ephemeral_sqlite().await.unwrap();
        let mut evaluation = base_evaluation(ReversibilityClass::Irreversible);
        evaluation.dry_run_complete = false;
        evaluation.auditor_attempted = false;
        evaluation.should_block = true;
        evaluation.decision = DryRunGateDecision::Block;
        evaluation.block_reason = Some("missing dry-run fields".to_string());
        let mut state = DryRunState::from_evaluation("a3-forged", &evaluation);
        state.should_block = false;
        let err = run_dry_run_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_block"), "{err}");
    }
}
