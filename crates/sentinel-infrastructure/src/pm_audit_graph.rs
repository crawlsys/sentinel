//! Graph-backed PM audit flag authorization.
//!
//! The offline Linear PM audit emits discipline flags. This graph is the
//! checkpointed LangGraph authority that classifies each flag as a hard PM
//! violation, an advisory PM warning, or a clear/no-op result. CLI and MCP
//! callers use the resulting checkpoint, write-history, stream, and topology
//! evidence instead of returning scanner flags as uncheckpointed facts.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::linear_pm_audit::PmFlag;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PmAuditDecision {
    /// No PM action is authorized.
    #[default]
    Clear,
    /// Advisory PM warning: visible, checkpointed, but not a hard gate.
    Advisory,
    /// Hard PM violation: the same class of discipline issue the live gate
    /// must block or escalate.
    HardViolation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PmAuditState {
    pub identifier: String,
    pub title: String,
    pub category: String,
    pub estimate: Option<f64>,
    pub state: String,
    pub detail: String,
    pub hard_gate_candidate: bool,
    pub decision: PmAuditDecision,
}

impl PmAuditState {
    #[must_use]
    pub fn from_flag(flag: &PmFlag) -> Self {
        Self {
            identifier: flag.identifier.clone(),
            title: flag.title.clone(),
            category: flag.category.clone(),
            estimate: flag.estimate,
            state: flag.state.clone(),
            detail: flag.detail.clone(),
            hard_gate_candidate: is_hard_pm_category(&flag.category),
            decision: PmAuditDecision::Clear,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PmAuditGraphRun {
    pub state: PmAuditState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<PmAuditState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

/// Proof that a PM audit graph checkpoint authorized surfacing a flag.
#[derive(Debug, Clone)]
pub struct PmAuditFlagAuthorization {
    identifier: String,
    decision: PmAuditDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl PmAuditFlagAuthorization {
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    #[must_use]
    pub fn decision(&self) -> PmAuditDecision {
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

impl PmAuditGraphRun {
    #[must_use]
    pub fn flag_authorization(&self) -> Result<Option<PmAuditFlagAuthorization>, String> {
        if self.state.decision == PmAuditDecision::Clear {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "pm_audit",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(PmAuditFlagAuthorization {
            identifier: self.state.identifier.clone(),
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const HARD: &str = "hard_violation";
const ADVISORY: &str = "advisory";
const CLEAR: &str = "clear";

pub type PmAuditGraph = CompilationResult<PmAuditState>;

#[must_use]
pub fn pm_audit_decision_label(decision: PmAuditDecision) -> &'static str {
    match decision {
        PmAuditDecision::Clear => "clear",
        PmAuditDecision::Advisory => "advisory",
        PmAuditDecision::HardViolation => "hard-violation",
    }
}

fn is_hard_pm_category(category: &str) -> bool {
    matches!(
        category,
        "missing-estimate" | "oversized" | "blocked" | "no-milestone"
    )
}

fn is_advisory_pm_category(category: &str) -> bool {
    matches!(category, "non-fibonacci" | "qa-failed")
}

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "pm_audit")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn pm_audit_state_schema() -> StateSchema<PmAuditState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "title",
                "category",
                "estimate",
                "state",
                "detail",
                "hard_gate_candidate",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "title": { "type": "string" },
                "category": { "type": "string", "minLength": 1 },
                "estimate": {
                    "anyOf": [
                        { "type": "null" },
                        { "type": "number", "exclusiveMinimum": 0 }
                    ]
                },
                "state": { "type": "string" },
                "detail": { "type": "string", "minLength": 1 },
                "hard_gate_candidate": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Clear", "Advisory", "HardViolation"]
                }
            },
            "x-sentinel": {
                "graph": "pm_audit",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &PmAuditState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "pm_audit identifier must not be empty".to_string(),
                ));
            }
            if state.category.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "pm_audit category must not be empty".to_string(),
                ));
            }
            if state.detail.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "pm_audit detail must not be empty".to_string(),
                ));
            }
            if state
                .estimate
                .is_some_and(|estimate| estimate <= 0.0 || !estimate.is_finite())
            {
                return Err(StateError::ValidationFailed(
                    "pm_audit estimate must be positive and finite when present".to_string(),
                ));
            }
            let hard_category = is_hard_pm_category(&state.category);
            let advisory_category = is_advisory_pm_category(&state.category);
            if state.hard_gate_candidate != hard_category {
                return Err(StateError::ValidationFailed(
                    "pm_audit hard_gate_candidate must match the category".to_string(),
                ));
            }
            match state.decision {
                PmAuditDecision::HardViolation if !hard_category => {
                    Err(StateError::ValidationFailed(
                        "pm_audit HardViolation requires a hard PM category".to_string(),
                    ))
                }
                PmAuditDecision::Advisory if hard_category || !advisory_category => {
                    Err(StateError::ValidationFailed(
                        "pm_audit Advisory requires an advisory PM category".to_string(),
                    ))
                }
                _ => Ok(()),
            }
        })
}

pub async fn build_pm_audit_graph() -> Result<PmAuditGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("pm_audit").await?;
    build_pm_audit_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_pm_audit_graph_with_ephemeral_sqlite() -> Result<PmAuditGraph, String> {
    build_pm_audit_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_pm_audit_graph_with_database_path(
    database_path: &str,
) -> Result<PmAuditGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_pm_audit_graph_with_checkpointer(checkpointer).await
}

async fn build_pm_audit_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<PmAuditGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = pm_audit_state_schema();
    let builder = StateGraphBuilder::<PmAuditState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: PmAuditState| async move {
                emit_decision_node_event("pm_audit", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            HARD,
            |s: PmAuditState| async move {
                emit_decision_node_event("pm_audit", HARD, &s.identifier)?;
                let mut next = s;
                next.decision = PmAuditDecision::HardViolation;
                Ok::<_, NodeError>(next)
            },
            node_config(HARD, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ADVISORY,
            |s: PmAuditState| async move {
                emit_decision_node_event("pm_audit", ADVISORY, &s.identifier)?;
                let mut next = s;
                next.decision = PmAuditDecision::Advisory;
                Ok::<_, NodeError>(next)
            },
            node_config(ADVISORY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            CLEAR,
            |s: PmAuditState| async move {
                emit_decision_node_event("pm_audit", CLEAR, &s.identifier)?;
                let mut next = s;
                next.decision = PmAuditDecision::Clear;
                Ok::<_, NodeError>(next)
            },
            node_config(CLEAR, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &PmAuditState| {
            if is_hard_pm_category(&s.category) {
                HARD.into()
            } else if is_advisory_pm_category(&s.category) {
                ADVISORY.into()
            } else {
                CLEAR.into()
            }
        })
        .add_edge(HARD, END)
        .add_edge(ADVISORY, END)
        .add_edge(CLEAR, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_pm_audit_decision_report(
    compiled: &PmAuditGraph,
    state: PmAuditState,
) -> Result<PmAuditGraphRun, String> {
    let thread_id =
        crate::decision_graph_store::run_thread_id("pm_audit", &state.identifier, &state)?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "pm_audit", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "pm_audit",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(PmAuditGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: pm_audit_graph_topology(compiled)?,
    })
}

pub fn pm_audit_graph_topology(compiled: &PmAuditGraph) -> Result<DecisionGraphTopology, String> {
    topology("pm_audit", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flag(category: &str) -> PmFlag {
        PmFlag {
            identifier: "FPCRM-1".to_string(),
            title: "PM issue".to_string(),
            category: category.to_string(),
            estimate: Some(8.0),
            state: "Backlog".to_string(),
            detail: "audit flag".to_string(),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_hard_pm_violation() {
        let graph = build_pm_audit_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = PmAuditState::from_flag(&flag("oversized"));
        let run = run_pm_audit_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, PmAuditDecision::HardViolation);
        assert_eq!(run.topology.graph, "pm_audit");
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert!(
            run.stream
                .iter()
                .any(|part| part.event_type == "ExecutionComplete"),
            "stream must expose LangGraph execution completion"
        );
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state write history"
        );
        assert!(
            run.write_history
                .iter()
                .filter(|write| write.channel == "state")
                .any(|write| write.value_json["decision"] == "HardViolation"),
            "state write history must decode the terminal decision JSON"
        );
        let authorization = run
            .flag_authorization()
            .expect("hard PM violation should have checkpoint authorization")
            .expect("authorization");
        assert_eq!(authorization.identifier(), "FPCRM-1");
        assert_eq!(authorization.decision(), PmAuditDecision::HardViolation);
        assert_eq!(authorization.thread_id(), run.thread_id);
        assert!(authorization.checkpoint_ref().contains('#'));
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let graph = build_pm_audit_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = PmAuditState::from_flag(&flag("oversized"));
        let mut run = run_pm_audit_decision_report(&graph, state)
            .await
            .expect("graph runs");
        run.write_history.clear();

        let err = run
            .flag_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_authorizes_advisory_pm_warning() {
        let graph = build_pm_audit_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let state = PmAuditState::from_flag(&flag("qa-failed"));
        let run = run_pm_audit_decision_report(&graph, state)
            .await
            .expect("graph runs");
        assert_eq!(run.state.decision, PmAuditDecision::Advisory);
        assert_eq!(
            run.flag_authorization()
                .expect("advisory should have checkpoint authorization")
                .expect("authorization")
                .decision(),
            PmAuditDecision::Advisory
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_clear_for_known_flag() {
        let graph = build_pm_audit_graph_with_ephemeral_sqlite()
            .await
            .expect("graph builds");
        let mut state = PmAuditState::from_flag(&flag("oversized"));
        state.hard_gate_candidate = false;
        let err = run_pm_audit_decision_report(&graph, state)
            .await
            .expect_err("forged hard candidate should fail schema validation");
        assert!(err.contains("hard_gate_candidate"));
    }
}
