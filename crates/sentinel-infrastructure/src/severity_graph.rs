//! Graph-backed Linear priority gap-fill for auto-severity proposals.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::severity::SeverityProposal;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SeverityMutationDecision {
    Set,
    #[default]
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeverityMutationState {
    pub identifier: String,
    pub issue_id: Option<String>,
    pub action: String,
    pub current_priority: Option<i64>,
    pub proposed_priority: i64,
    pub models_agreed: bool,
    pub decision: SeverityMutationDecision,
}

impl SeverityMutationState {
    #[must_use]
    pub fn from_proposal(proposal: &SeverityProposal) -> Self {
        Self {
            identifier: proposal.identifier.clone(),
            issue_id: proposal.issue_id.clone(),
            action: proposal.action.clone(),
            current_priority: proposal.current_priority,
            proposed_priority: proposal.proposed_priority,
            models_agreed: proposal.models_agreed,
            decision: SeverityMutationDecision::Skip,
        }
    }

    fn can_set_priority(&self) -> bool {
        self.action == "set"
            && self.issue_id.as_deref().is_some_and(|id| !id.is_empty())
            && self.current_priority.is_none_or(|p| p <= 0)
            && (1..=4).contains(&self.proposed_priority)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SeverityMutationRun {
    pub state: SeverityMutationState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<SeverityMutationState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

/// Proof that a severity graph checkpoint authorized a Linear priority update.
#[derive(Debug, Clone)]
pub struct SeverityApplyAuthorization {
    identifier: String,
    issue_id: String,
    proposed_priority: i64,
    thread_id: String,
    checkpoint_id: String,
}

impl SeverityApplyAuthorization {
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    #[must_use]
    pub fn issue_id(&self) -> &str {
        &self.issue_id
    }

    #[must_use]
    pub fn proposed_priority(&self) -> i64 {
        self.proposed_priority
    }

    /// Durable severity graph thread that authorized the mutation.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Durable severity graph checkpoint that authorized the mutation.
    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    /// Stable audit reference for this concrete authorization checkpoint.
    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl SeverityMutationRun {
    /// Convert a terminal `Set` graph result into a Linear priority authorization.
    #[must_use]
    pub fn apply_authorization(&self) -> Result<Option<SeverityApplyAuthorization>, String> {
        if self.state.decision != SeverityMutationDecision::Set {
            return Ok(None);
        }
        let issue_id = self.state.issue_id.as_ref().cloned().ok_or_else(|| {
            format!(
                "severity graph Set decision for '{}' omitted issue_id evidence",
                self.state.identifier
            )
        })?;
        let checkpoint_id = terminal_decision_checkpoint_result(
            "severity",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(SeverityApplyAuthorization {
            identifier: self.state.identifier.clone(),
            issue_id,
            proposed_priority: self.state.proposed_priority,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

#[derive(Debug, Clone)]
pub struct SeverityApplyResult {
    pub run: SeverityMutationRun,
    pub authorization: Option<SeverityApplyAuthorization>,
    pub applied: bool,
}

const CLASSIFY: &str = "classify";
const SET: &str = "set_priority";
const SKIP: &str = "skip";

pub type SeverityMutationGraph = CompilationResult<SeverityMutationState>;

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "severity")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn severity_mutation_state_schema() -> StateSchema<SeverityMutationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "issue_id",
                "action",
                "current_priority",
                "proposed_priority",
                "models_agreed",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "issue_id": {
                    "anyOf": [
                        { "type": "null" },
                        { "type": "string", "minLength": 1 }
                    ]
                },
                "action": { "type": "string", "minLength": 1 },
                "current_priority": {
                    "anyOf": [
                        { "type": "null" },
                        { "type": "integer", "minimum": 0, "maximum": 4 }
                    ]
                },
                "proposed_priority": { "type": "integer", "minimum": 1, "maximum": 4 },
                "models_agreed": { "type": "boolean" },
                "decision": { "type": "string", "enum": ["Set", "Skip"] }
            },
            "x-sentinel": {
                "graph": "severity",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &SeverityMutationState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "severity identifier must not be empty".to_string(),
                ));
            }
            if state.action.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "severity action must not be empty".to_string(),
                ));
            }
            if !(1..=4).contains(&state.proposed_priority) {
                return Err(StateError::ValidationFailed(
                    "severity proposed_priority must be between 1 and 4".to_string(),
                ));
            }
            if state
                .current_priority
                .is_some_and(|priority| !(0..=4).contains(&priority))
            {
                return Err(StateError::ValidationFailed(
                    "severity current_priority must be between 0 and 4 when present"
                        .to_string(),
                ));
            }
            if state.decision == SeverityMutationDecision::Set && !state.can_set_priority() {
                return Err(StateError::ValidationFailed(
                    "severity Set requires a set action, issue id, empty current priority, and valid proposed priority"
                        .to_string(),
                ));
            }
            Ok(())
        })
}

pub async fn build_severity_mutation_graph() -> Result<SeverityMutationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("severity").await?;
    build_severity_mutation_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_severity_mutation_graph_with_ephemeral_sqlite(
) -> Result<SeverityMutationGraph, String> {
    build_severity_mutation_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_severity_mutation_graph_with_database_path(
    database_path: &str,
) -> Result<SeverityMutationGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_severity_mutation_graph_with_checkpointer(checkpointer).await
}

async fn build_severity_mutation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<SeverityMutationGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = severity_mutation_state_schema();
    let builder = StateGraphBuilder::<SeverityMutationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: SeverityMutationState| async move {
                emit_decision_node_event("severity", CLASSIFY, &s.identifier)?;
                Ok::<_, NodeError>(s)
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
            SET,
            |s: SeverityMutationState| async move {
                emit_decision_node_event("severity", SET, &s.identifier)?;
                let mut next = s;
                next.decision = SeverityMutationDecision::Set;
                Ok::<_, NodeError>(next)
            },
            node_config(
                SET,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            SKIP,
            |s: SeverityMutationState| async move {
                emit_decision_node_event("severity", SKIP, &s.identifier)?;
                let mut next = s;
                next.decision = SeverityMutationDecision::Skip;
                Ok::<_, NodeError>(next)
            },
            node_config(
                SKIP,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(CLASSIFY, |s: &SeverityMutationState| {
            if s.can_set_priority() {
                SET.into()
            } else {
                SKIP.into()
            }
        })
        .add_edge(SET, END)
        .add_edge(SKIP, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_severity_mutation_decision_report(
    compiled: &SeverityMutationGraph,
    state: SeverityMutationState,
) -> Result<SeverityMutationRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "severity",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "severity", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "severity",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(SeverityMutationRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: severity_mutation_graph_topology(compiled)?,
    })
}

pub fn severity_mutation_graph_topology(
    compiled: &SeverityMutationGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("severity", compiled)
}

pub async fn apply_severity_proposal(
    client: &reqwest::Client,
    token: &str,
    compiled: &SeverityMutationGraph,
    proposal: &SeverityProposal,
) -> Result<SeverityApplyResult, String> {
    let state = SeverityMutationState::from_proposal(proposal);
    let run = run_severity_mutation_decision_report(compiled, state).await?;
    if run.state.decision != SeverityMutationDecision::Set {
        return Ok(SeverityApplyResult {
            run,
            authorization: None,
            applied: false,
        });
    }

    let authorization = run
        .apply_authorization()
        .map_err(|e| format!("severity graph authorization failed: {e}"))?
        .ok_or_else(|| "severity graph authorized set without checkpoint token".to_string())?;
    set_priority(client, token, &authorization).await?;
    Ok(SeverityApplyResult {
        run,
        authorization: Some(authorization),
        applied: true,
    })
}

async fn set_priority(
    client: &reqwest::Client,
    token: &str,
    authorization: &SeverityApplyAuthorization,
) -> Result<(), String> {
    let issue_id = authorization.issue_id();
    let priority = authorization.proposed_priority();
    tracing::info!(
        ticket = %authorization.identifier(),
        issue = %issue_id,
        priority,
        severity_graph_thread_id = %authorization.thread_id(),
        severity_graph_checkpoint_id = %authorization.checkpoint_id(),
        "sentinel severity: priority mutation authorized by LangGraph"
    );
    let mutation = serde_json::json!({
        "query": "mutation($id:String!,$p:Int!){issueUpdate(id:$id,input:{priority:$p}){success}}",
        "variables": { "id": issue_id, "p": priority }
    });
    let response = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .json(&mutation)
        .send()
        .await
        .map_err(|e| format!("Linear priority mutation request failed for {issue_id}: {e}"))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read Linear priority mutation response for {issue_id}: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "Linear priority mutation for {issue_id} returned HTTP {status}: {body}"
        ));
    }

    let parsed: LinearMutationResponse = serde_json::from_str(&body)
        .map_err(|e| format!("parse Linear priority mutation response for {issue_id}: {e}"))?;
    if parsed
        .errors
        .as_ref()
        .is_some_and(|errors| !errors.is_empty())
    {
        return Err(format!(
            "Linear priority mutation for {issue_id} returned GraphQL errors: {}",
            serde_json::to_string(&parsed.errors).unwrap_or_else(|_| "unserializable".into())
        ));
    }
    match parsed
        .data
        .and_then(|data| data.issue_update)
        .map(|update| update.success)
    {
        Some(true) => Ok(()),
        Some(false) => Err(format!(
            "Linear priority mutation for {issue_id} returned success=false"
        )),
        None => Err(format!(
            "Linear priority mutation for {issue_id} omitted issueUpdate.success"
        )),
    }
}

#[derive(Debug, Deserialize)]
struct LinearMutationResponse {
    data: Option<LinearMutationData>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct LinearMutationData {
    #[serde(rename = "issueUpdate")]
    issue_update: Option<LinearIssueUpdate>,
}

#[derive(Debug, Deserialize)]
struct LinearIssueUpdate {
    success: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(
        action: &str,
        current_priority: Option<i64>,
        issue_id: Option<&str>,
    ) -> SeverityProposal {
        SeverityProposal {
            issue_id: issue_id.map(str::to_string),
            identifier: "FPCRM-1".into(),
            title: "prod down".into(),
            current_priority,
            proposed_priority: 1,
            reasoning: "outage".into(),
            action: action.into(),
            opus_priority: 1,
            gpt_priority: 1,
            models_agreed: true,
        }
    }

    #[tokio::test]
    async fn graph_authorizes_set_for_unprioritized_ticket() {
        let graph = build_severity_mutation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = SeverityMutationState::from_proposal(&proposal("set", Some(0), Some("uuid-1")));
        let run = run_severity_mutation_decision_report(&graph, state)
            .await
            .expect("runs");
        assert_eq!(run.state.decision, SeverityMutationDecision::Set);
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert_eq!(
            run.checkpoints.first().expect("latest").thread_id,
            run.thread_id
        );
        assert_eq!(
            run.checkpoints.first().expect("latest").state.decision,
            SeverityMutationDecision::Set
        );
        assert!(!run.stream.is_empty(), "run must expose stream parts");
        assert!(
            run.stream
                .iter()
                .any(|part| part.event_type == "ExecutionComplete"),
            "stream must expose LangGraph execution completion"
        );
        assert!(
            run.stream.iter().all(|part| part.stream_protocol == "v3"),
            "stream must expose the LangGraph v3 typed protocol"
        );
        assert!(
            run.stream.iter().any(|part| part.payload_kind == "values"),
            "stream must expose LangGraph values payloads"
        );
        assert!(
            run.stream.iter().any(|part| part.payload_kind == "updates"),
            "stream must expose LangGraph v3 update payloads"
        );
        assert!(
            run.stream.iter().any(|part| part.payload_kind == "tasks"),
            "stream must expose LangGraph v3 task payloads"
        );
        assert!(
            run.stream
                .iter()
                .any(|part| part.payload_kind == "checkpoints"),
            "stream must expose LangGraph checkpoint payloads"
        );
        assert!(
            run.stream.iter().any(|part| {
                part.payload_kind == "custom"
                    && part.payload_json["type"] == "sentinel.decision_node"
                    && part.payload_json["graph"] == "severity"
            }),
            "stream must expose Sentinel custom decision-node payloads"
        );
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state channel write history"
        );
        assert!(
            run.write_history
                .iter()
                .all(|write| write.value_len > 0 && write.value_sha256.len() == 64),
            "write history must expose value length and sha256"
        );
        assert!(
            run.write_history
                .iter()
                .filter(|write| write.channel == "state")
                .any(|write| write.value_json["decision"] == "Set"),
            "state write history must decode the terminal decision JSON"
        );
        let auth = run
            .apply_authorization()
            .expect("Set run should authorize priority mutation")
            .expect("authorization");
        assert_eq!(auth.identifier(), "FPCRM-1");
        assert_eq!(auth.issue_id(), "uuid-1");
        assert_eq!(auth.proposed_priority(), 1);
        assert_eq!(auth.thread_id(), run.thread_id);
        let auth_checkpoint = run
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == auth.checkpoint_id())
            .expect("authorization checkpoint must be present");
        assert_eq!(auth_checkpoint.source_node.as_deref(), Some(SET));
        assert_eq!(
            auth_checkpoint
                .writes
                .iter()
                .find(|write| write.channel == "state")
                .expect("authorization checkpoint state write")
                .node_id
                .as_str(),
            SET
        );
        assert_eq!(auth.checkpoint_ref(), auth_checkpoint.checkpoint_ref());
        assert_eq!(run.topology.graph, "severity");
        assert!(run.topology.durable_checkpointer);
        assert_eq!(run.topology.checkpointer_backend, "sqlite");
        assert!(
            run.topology
                .checkpointer_scope
                .starts_with("database_path:"),
            "topology must expose sanitized checkpoint scope"
        );
        assert_eq!(
            run.topology.schemas.state.as_ref().expect("state schema")["x-sentinel"]["graph"],
            "severity"
        );
        assert!(run.topology.schemas.input.is_some());
        assert!(run.topology.schemas.output.is_some());
        assert!(
            run.topology
                .edges
                .iter()
                .any(|edge| edge.kind == "conditional"),
            "topology must expose set/skip conditional routing"
        );
        let serialized = serde_json::to_value(&run).expect("severity graph run serializes");
        assert_eq!(serialized["topology"]["graph"], "severity");
        assert_eq!(serialized["topology"]["checkpointer_backend"], "sqlite");
        assert!(serialized["topology"]["checkpointer_scope"]
            .as_str()
            .expect("serialized scope")
            .starts_with("database_path:"));
        assert!(
            serialized["stream"]
                .as_array()
                .expect("serialized stream")
                .iter()
                .any(|part| part["event_type"] == "ExecutionComplete"),
            "serialized run must preserve stream evidence"
        );
        assert!(
            !serialized["checkpoints"]
                .as_array()
                .expect("serialized checkpoints")
                .is_empty(),
            "serialized run must preserve checkpoints"
        );
        assert!(
            serialized["write_history"]
                .as_array()
                .expect("serialized write history")
                .iter()
                .any(|write| write["channel"] == "state"),
            "serialized run must preserve state write history"
        );
    }

    #[tokio::test]
    async fn graph_skips_suggestions_missing_ids_and_existing_priority() {
        let graph = build_severity_mutation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        for state in [
            SeverityMutationState::from_proposal(&proposal("suggest", Some(4), Some("uuid-1"))),
            SeverityMutationState::from_proposal(&proposal("set", Some(0), None)),
            SeverityMutationState::from_proposal(&proposal("set", Some(2), Some("uuid-1"))),
        ] {
            let run = run_severity_mutation_decision_report(&graph, state)
                .await
                .expect("runs");
            assert_eq!(run.state.decision, SeverityMutationDecision::Skip);
            assert!(run
                .apply_authorization()
                .expect("authorization result")
                .is_none());
        }
    }

    #[tokio::test]
    async fn graph_schema_rejects_invalid_priority_value() {
        let graph = build_severity_mutation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut invalid_priority = proposal("set", Some(0), Some("uuid-1"));
        invalid_priority.proposed_priority = 7;

        let err = run_severity_mutation_decision_report(
            &graph,
            SeverityMutationState::from_proposal(&invalid_priority),
        )
        .await
        .expect_err("out-of-range priority must fail LangGraph schema validation");
        assert!(
            err.contains("proposed_priority must be between 1 and 4"),
            "unexpected error: {err}"
        );
    }
}
