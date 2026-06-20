//! Graph-backed agent revocation authorization.
//!
//! The application hook computes deterministic facts for tool calls carrying
//! an `agent_id`: whether an agent id is present and whether it is revoked in
//! the current session state. This graph authorizes the resulting allow/deny
//! decision through durable LangGraph checkpoints before the CLI permits the
//! tool call to proceed.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::agent_revocation::AgentRevocationEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AgentRevocationDecision {
    #[default]
    Unclassified,
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRevocationState {
    pub identifier: String,
    pub tool: Option<String>,
    pub agent_id_present: bool,
    pub agent_id_sha256: Option<String>,
    pub revoked: bool,
    pub blocking_finding_count: u64,
    pub should_deny: bool,
    pub decision: AgentRevocationDecision,
}

impl AgentRevocationState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &AgentRevocationEvaluation,
    ) -> Self {
        let agent_id_sha256 = evaluation
            .agent_id
            .as_deref()
            .filter(|_| evaluation.agent_id_present)
            .map(sha256);
        let blocking_finding_count = u64::from(expected_should_deny(
            evaluation.agent_id_present,
            evaluation.revoked,
        ));
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            agent_id_present: evaluation.agent_id_present,
            agent_id_sha256,
            revoked: evaluation.revoked,
            blocking_finding_count,
            should_deny: blocking_finding_count > 0,
            decision: AgentRevocationDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentRevocationGraphRun {
    pub state: AgentRevocationState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<AgentRevocationState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct AgentRevocationAuthorization {
    decision: AgentRevocationDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl AgentRevocationAuthorization {
    #[must_use]
    pub fn decision(&self) -> AgentRevocationDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl AgentRevocationGraphRun {
    #[must_use]
    pub fn agent_revocation_authorization(
        &self,
    ) -> Result<Option<AgentRevocationAuthorization>, String> {
        if self.state.decision == AgentRevocationDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "agent_revocation",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(AgentRevocationAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const DENY: &str = "deny";

pub type AgentRevocationGraph = CompilationResult<AgentRevocationState>;

#[must_use]
pub fn agent_revocation_decision_label(decision: AgentRevocationDecision) -> &'static str {
    match decision {
        AgentRevocationDecision::Unclassified => "unclassified",
        AgentRevocationDecision::Allow => "allow",
        AgentRevocationDecision::Deny => "deny",
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

fn expected_should_deny(agent_id_present: bool, revoked: bool) -> bool {
    agent_id_present && revoked
}

fn expected_decision(state: &AgentRevocationState) -> AgentRevocationDecision {
    if state.should_deny {
        AgentRevocationDecision::Deny
    } else {
        AgentRevocationDecision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "agent_revocation")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn agent_revocation_state_schema() -> StateSchema<AgentRevocationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "agent_id_present",
                "agent_id_sha256",
                "revoked",
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
                "agent_id_present": { "type": "boolean" },
                "agent_id_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "revoked": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_deny": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Allow", "Deny"]
                }
            },
            "x-sentinel": {
                "graph": "agent_revocation",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &AgentRevocationState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if !state.agent_id_present {
                if state.agent_id_sha256.is_some()
                    || state.revoked
                    || state.blocking_finding_count > 0
                    || state.should_deny
                {
                    return Err(StateError::ValidationFailed(
                        "agent_revocation missing-agent state cannot carry revocation facts"
                            .to_string(),
                    ));
                }
            } else if !optional_hex_digest_present(&state.agent_id_sha256) {
                return Err(StateError::ValidationFailed(
                    "agent_revocation agent_id_sha256 must be a 64-character hex digest"
                        .to_string(),
                ));
            }
            let expected_should_deny = expected_should_deny(state.agent_id_present, state.revoked);
            if state.should_deny != expected_should_deny {
                return Err(StateError::ValidationFailed(format!(
                    "agent_revocation should_deny must match revocation policy: expected \
                     {expected_should_deny}, got {}",
                    state.should_deny
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_deny);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "agent_revocation blocking_finding_count must match should_deny: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != AgentRevocationDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "agent_revocation terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: AgentRevocationState) -> Result<AgentRevocationState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_agent_revocation_graph() -> Result<AgentRevocationGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("agent_revocation").await?;
    build_agent_revocation_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_agent_revocation_graph_with_ephemeral_sqlite() -> Result<AgentRevocationGraph, String>
{
    build_agent_revocation_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_agent_revocation_graph_with_database_path(
    db_path: &str,
) -> Result<AgentRevocationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_agent_revocation_graph_with_checkpointer(checkpointer).await
}

async fn build_agent_revocation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<AgentRevocationGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = agent_revocation_state_schema();
    let builder = StateGraphBuilder::<AgentRevocationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: AgentRevocationState| async move {
                emit_decision_node_event("agent_revocation", CLASSIFY, &s.identifier)?;
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
            |s: AgentRevocationState| async move {
                emit_decision_node_event("agent_revocation", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = AgentRevocationDecision::Allow;
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
            DENY,
            |s: AgentRevocationState| async move {
                emit_decision_node_event("agent_revocation", DENY, &s.identifier)?;
                let mut next = s;
                next.decision = AgentRevocationDecision::Deny;
                Ok::<_, NodeError>(next)
            },
            node_config(
                DENY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &AgentRevocationState| match expected_decision(s) {
                AgentRevocationDecision::Allow => ALLOW.into(),
                AgentRevocationDecision::Deny => DENY.into(),
                AgentRevocationDecision::Unclassified => ALLOW.into(),
            },
        )
        .add_edge(ALLOW, END)
        .add_edge(DENY, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_agent_revocation_decision_report(
    compiled: &AgentRevocationGraph,
    state: AgentRevocationState,
) -> Result<AgentRevocationGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "agent_revocation",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "agent_revocation", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "agent_revocation",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(AgentRevocationGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: agent_revocation_graph_topology(compiled)?,
    })
}

pub fn agent_revocation_graph_topology(
    compiled: &AgentRevocationGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("agent_revocation", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::agent_revocation::{
        AgentRevocationDecision as AppDecision, AgentRevocationEvaluation,
    };

    fn evaluation(revoked: bool) -> AgentRevocationEvaluation {
        AgentRevocationEvaluation {
            tool: Some("Bash".to_string()),
            agent_id: Some("agent-abc".to_string()),
            agent_id_present: true,
            revoked,
            should_deny: revoked,
            decision: if revoked {
                AppDecision::Deny
            } else {
                AppDecision::Allow
            },
        }
    }

    fn missing_agent_evaluation() -> AgentRevocationEvaluation {
        AgentRevocationEvaluation {
            tool: Some("Bash".to_string()),
            agent_id: None,
            agent_id_present: false,
            revoked: false,
            should_deny: false,
            decision: AppDecision::Allow,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_revoked_agent_deny() {
        let graph = build_agent_revocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            AgentRevocationState::from_evaluation("agent-revocation-deny", &evaluation(true));
        assert!(state.agent_id_sha256.is_some());
        let run = run_agent_revocation_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, AgentRevocationDecision::Deny);
        assert!(run
            .agent_revocation_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_active_agent_allow() {
        let graph = build_agent_revocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state =
            AgentRevocationState::from_evaluation("agent-revocation-allow", &evaluation(false));
        let run = run_agent_revocation_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, AgentRevocationDecision::Allow);
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_allow_for_revoked_agent() {
        let graph = build_agent_revocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state =
            AgentRevocationState::from_evaluation("agent-revocation-forged", &evaluation(true));
        state.should_deny = false;
        state.blocking_finding_count = 0;
        let err = run_agent_revocation_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_deny"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_revocation_without_agent_id() {
        let graph = build_agent_revocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(true);
        eval.agent_id = None;
        eval.agent_id_present = false;
        let state = AgentRevocationState::from_evaluation("agent-revocation-missing-agent", &eval);
        let err = run_agent_revocation_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing-agent"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_agent_without_agent_digest() {
        let graph = build_agent_revocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(true);
        eval.agent_id = None;
        eval.agent_id_present = true;
        let state = AgentRevocationState::from_evaluation("agent-revocation-missing-digest", &eval);

        let err = run_agent_revocation_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("agent_id_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_agent_with_extra_agent_digest() {
        let graph = build_agent_revocation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = AgentRevocationState::from_evaluation(
            "agent-revocation-extra-digest",
            &missing_agent_evaluation(),
        );
        state.agent_id_sha256 = Some(sha256("ghost-agent"));

        let err = run_agent_revocation_decision_report(&graph, state)
            .await
            .unwrap_err();

        assert!(err.contains("missing-agent"), "{err}");
    }
}
