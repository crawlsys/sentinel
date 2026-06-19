//! Graph-backed capability routing authorization.
//!
//! `sentinel__route_capability` is an MCP-visible authority surface: it tells
//! the agent which registered worker should handle work. The pure router still
//! computes the explanation, but this graph validates the explanation shape and
//! persists a durable LangGraph checkpoint before MCP returns it.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_domain::agent_routing::{RequirementSignature, RoutingExplanation};
use sentinel_domain::capability::CapabilityRequirement;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CapabilityRouteDecision {
    #[default]
    Unclassified,
    Routed,
    NoRoute,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRouteState {
    pub identifier: String,
    pub requirement_signature: String,
    pub explanation_requirement_signature: String,
    pub requirement_sha256: String,
    pub explanation_sha256: String,
    pub profile_count: u64,
    pub chosen_present: bool,
    pub chosen_sha256: Option<String>,
    pub chosen_in_candidates: bool,
    pub candidate_count: u64,
    pub eliminated_count: u64,
    pub tie_breaker_count: u64,
    pub decision: CapabilityRouteDecision,
}

impl CapabilityRouteState {
    #[must_use]
    pub fn from_explanation(
        identifier: impl Into<String>,
        requirement: &CapabilityRequirement,
        explanation: &RoutingExplanation,
        profile_count: usize,
    ) -> Self {
        let chosen_sha256 = explanation
            .chosen
            .as_ref()
            .map(|agent| sha256(agent.as_str()))
            .filter(|digest| !digest.is_empty());
        let chosen_in_candidates = explanation
            .chosen
            .as_ref()
            .is_some_and(|chosen| explanation.candidates.iter().any(|agent| agent == chosen));
        Self {
            identifier: identifier.into(),
            requirement_signature: RequirementSignature::of(requirement).to_string(),
            explanation_requirement_signature: explanation.requirement_signature.to_string(),
            requirement_sha256: sha256_json(requirement),
            explanation_sha256: sha256_json(explanation),
            profile_count: profile_count as u64,
            chosen_present: explanation.chosen.is_some(),
            chosen_sha256,
            chosen_in_candidates,
            candidate_count: explanation.candidates.len() as u64,
            eliminated_count: explanation.eliminated.len() as u64,
            tie_breaker_count: explanation.tie_breakers_applied.len() as u64,
            decision: CapabilityRouteDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilityRouteGraphRun {
    pub state: CapabilityRouteState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<CapabilityRouteState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct CapabilityRouteAuthorization {
    decision: CapabilityRouteDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl CapabilityRouteAuthorization {
    #[must_use]
    pub fn decision(&self) -> CapabilityRouteDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl CapabilityRouteGraphRun {
    #[must_use]
    pub fn capability_route_authorization(
        &self,
    ) -> Result<Option<CapabilityRouteAuthorization>, String> {
        if self.state.decision == CapabilityRouteDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "capability_route",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(CapabilityRouteAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ROUTED: &str = "routed";
const NO_ROUTE: &str = "no_route";

pub type CapabilityRouteGraph = CompilationResult<CapabilityRouteState>;

#[must_use]
pub fn capability_route_decision_label(decision: CapabilityRouteDecision) -> &'static str {
    match decision {
        CapabilityRouteDecision::Unclassified => "unclassified",
        CapabilityRouteDecision::Routed => "routed",
        CapabilityRouteDecision::NoRoute => "no-route",
    }
}

#[must_use]
pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn sha256_json<T: Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value)
        .expect("capability route authority evidence must serialize to JSON");
    hex::encode(Sha256::digest(&bytes))
}

fn hex_digest_present(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn optional_hex_digest_present(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(hex_digest_present)
}

fn short_hex_present(value: &str) -> bool {
    value.len() == 16 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

fn expected_decision(state: &CapabilityRouteState) -> CapabilityRouteDecision {
    if state.chosen_present {
        CapabilityRouteDecision::Routed
    } else {
        CapabilityRouteDecision::NoRoute
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "capability_route")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn capability_route_state_schema() -> StateSchema<CapabilityRouteState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "requirement_signature",
                "explanation_requirement_signature",
                "requirement_sha256",
                "explanation_sha256",
                "profile_count",
                "chosen_present",
                "chosen_sha256",
                "chosen_in_candidates",
                "candidate_count",
                "eliminated_count",
                "tie_breaker_count",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "requirement_signature": { "type": "string", "minLength": 16, "maxLength": 16 },
                "explanation_requirement_signature": { "type": "string", "minLength": 16, "maxLength": 16 },
                "requirement_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "explanation_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                "profile_count": { "type": "integer", "minimum": 0 },
                "chosen_present": { "type": "boolean" },
                "chosen_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "chosen_in_candidates": { "type": "boolean" },
                "candidate_count": { "type": "integer", "minimum": 0 },
                "eliminated_count": { "type": "integer", "minimum": 0 },
                "tie_breaker_count": { "type": "integer", "minimum": 0 },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "Routed", "NoRoute"]
                }
            },
            "x-sentinel": {
                "graph": "capability_route",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &CapabilityRouteState| {
            if !short_hex_present(&state.requirement_signature)
                || !short_hex_present(&state.explanation_requirement_signature)
            {
                return Err(StateError::ValidationFailed(
                    "capability_route requirement signatures must be 16-character hex digests"
                        .to_string(),
                ));
            }
            if state.requirement_signature != state.explanation_requirement_signature {
                return Err(StateError::ValidationFailed(
                    "capability_route explanation signature must match requirement signature"
                        .to_string(),
                ));
            }
            if !hex_digest_present(&state.requirement_sha256)
                || !hex_digest_present(&state.explanation_sha256)
            {
                return Err(StateError::ValidationFailed(
                    "capability_route requirement/explanation hashes must be 64-character hex digests"
                        .to_string(),
                ));
            }
            if state.profile_count != state.candidate_count + state.eliminated_count {
                return Err(StateError::ValidationFailed(format!(
                    "capability_route profile accounting mismatch: profile_count={} candidate_count={} eliminated_count={}",
                    state.profile_count, state.candidate_count, state.eliminated_count
                )));
            }
            if state.chosen_present {
                if state.candidate_count == 0 || !state.chosen_in_candidates {
                    return Err(StateError::ValidationFailed(
                        "capability_route chosen agent must be present in candidates".to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.chosen_sha256) {
                    return Err(StateError::ValidationFailed(
                        "capability_route chosen agent requires 64-character digest".to_string(),
                    ));
                }
            } else {
                if state.chosen_in_candidates {
                    return Err(StateError::ValidationFailed(
                        "capability_route no-route decision cannot mark a chosen candidate"
                            .to_string(),
                    ));
                }
                if state.chosen_sha256.is_some() {
                    return Err(StateError::ValidationFailed(
                        "capability_route chosen digest without chosen agent".to_string(),
                    ));
                }
            }
            if state.decision != CapabilityRouteDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "capability_route terminal decision must match routing explanation: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: CapabilityRouteState) -> Result<CapabilityRouteState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

async fn terminal_node(
    state: CapabilityRouteState,
    decision: CapabilityRouteDecision,
) -> Result<CapabilityRouteState, NodeError> {
    let mut next = state;
    next.decision = decision;
    Ok(next)
}

pub async fn build_capability_route_graph() -> Result<CapabilityRouteGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("capability_route").await?;
    build_capability_route_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_capability_route_graph_with_ephemeral_sqlite() -> Result<CapabilityRouteGraph, String>
{
    build_capability_route_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_capability_route_graph_with_database_path(
    db_path: &str,
) -> Result<CapabilityRouteGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_capability_route_graph_with_checkpointer(checkpointer).await
}

async fn build_capability_route_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<CapabilityRouteGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = capability_route_state_schema();
    let builder = StateGraphBuilder::<CapabilityRouteState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: CapabilityRouteState| async move {
                emit_decision_node_event("capability_route", CLASSIFY, &s.identifier)?;
                classify_node(s).await
            },
            node_config(
                CLASSIFY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            ROUTED,
            |s: CapabilityRouteState| async move {
                emit_decision_node_event("capability_route", ROUTED, &s.identifier)?;
                terminal_node(s, CapabilityRouteDecision::Routed).await
            },
            node_config(
                ROUTED,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_async_node_with_config(
            NO_ROUTE,
            |s: CapabilityRouteState| async move {
                emit_decision_node_event("capability_route", NO_ROUTE, &s.identifier)?;
                terminal_node(s, CapabilityRouteDecision::NoRoute).await
            },
            node_config(
                NO_ROUTE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &CapabilityRouteState| match expected_decision(s) {
                CapabilityRouteDecision::Routed => ROUTED.into(),
                CapabilityRouteDecision::NoRoute | CapabilityRouteDecision::Unclassified => {
                    NO_ROUTE.into()
                }
            },
        )
        .add_edge(ROUTED, END)
        .add_edge(NO_ROUTE, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_capability_route_decision_report(
    compiled: &CapabilityRouteGraph,
    state: CapabilityRouteState,
) -> Result<CapabilityRouteGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "capability_route",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "capability_route", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "capability_route",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(CapabilityRouteGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: capability_route_graph_topology(compiled)?,
    })
}

pub fn capability_route_graph_topology(
    compiled: &CapabilityRouteGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("capability_route", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::capability::{AgentId, Capability, ReasoningLevel, SchemaRef};

    fn requirement() -> CapabilityRequirement {
        CapabilityRequirement {
            required: vec![
                Capability::Reasoning(ReasoningLevel::Standard),
                Capability::StructuredOutput(SchemaRef::AuditorVerdict),
            ],
            preferred: Vec::new(),
            forbidden: Vec::new(),
        }
    }

    fn routed_explanation(req: &CapabilityRequirement) -> RoutingExplanation {
        let chosen = AgentId::new("agent-a").unwrap();
        RoutingExplanation {
            chosen: Some(chosen.clone()),
            candidates: vec![chosen],
            eliminated: Vec::new(),
            tie_breakers_applied: Vec::new(),
            requirement_signature: RequirementSignature::of(req),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_routed_decision() {
        let req = requirement();
        let graph = build_capability_route_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = CapabilityRouteState::from_explanation(
            "cap-route-routed",
            &req,
            &routed_explanation(&req),
            1,
        );
        let run = run_capability_route_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, CapabilityRouteDecision::Routed);
        assert!(run
            .capability_route_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn authorization_surfaces_missing_checkpoint_write_history() {
        let req = requirement();
        let graph = build_capability_route_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = CapabilityRouteState::from_explanation(
            "cap-route-routed",
            &req,
            &routed_explanation(&req),
            1,
        );
        let mut run = run_capability_route_decision_report(&graph, state)
            .await
            .unwrap();
        run.write_history.clear();

        let err = run
            .capability_route_authorization()
            .expect_err("missing write history must surface as graph authorization error");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn graph_authorizes_no_route_decision() {
        let req = requirement();
        let graph = build_capability_route_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let explanation = RoutingExplanation {
            chosen: None,
            candidates: Vec::new(),
            eliminated: Vec::new(),
            tie_breakers_applied: Vec::new(),
            requirement_signature: RequirementSignature::of(&req),
        };
        let state = CapabilityRouteState::from_explanation("cap-route-none", &req, &explanation, 0);
        let run = run_capability_route_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, CapabilityRouteDecision::NoRoute);
    }

    #[tokio::test]
    async fn graph_schema_rejects_chosen_outside_candidates() {
        let req = requirement();
        let graph = build_capability_route_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = CapabilityRouteState::from_explanation(
            "cap-route-forged",
            &req,
            &routed_explanation(&req),
            1,
        );
        state.chosen_in_candidates = false;
        let err = run_capability_route_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("chosen"), "{err}");
    }
}
