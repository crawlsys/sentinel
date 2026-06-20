//! Graph-backed Doppler/Auth0 authorization.
//!
//! The application hook computes deterministic facts for Doppler and Auth0 MCP
//! tools: provider, operation class, autopilot state, production targeting, and
//! signed override status. This graph authorizes the resulting allow/block
//! decision through durable LangGraph checkpoints so secret and identity-provider
//! mutations cannot proceed from an uncheckpointed branch.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};

use sentinel_application::hooks::doppler_auth0_gate::{
    DopplerAuth0Evaluation, DopplerAuth0Provider,
};

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DopplerAuth0Decision {
    #[default]
    Unclassified,
    Allow,
    AllowReadOnly,
    AllowAutopilotNonProd,
    AllowSignedOverride,
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DopplerAuth0State {
    pub identifier: String,
    pub provider: String,
    pub operation: Option<String>,
    pub router_management: bool,
    pub read_only: bool,
    pub mutation: bool,
    pub autopilot: bool,
    pub tool_input_present: bool,
    pub production_target: bool,
    pub session_id_present: bool,
    pub signed_override_active: bool,
    pub auth0_override_supported: bool,
    pub blocking_finding_count: u64,
    pub should_block: bool,
    pub decision: DopplerAuth0Decision,
}

impl DopplerAuth0State {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &DopplerAuth0Evaluation,
    ) -> Self {
        let provider = provider_label(evaluation.provider).to_string();
        let operation = evaluation
            .operation
            .as_deref()
            .map(str::trim)
            .filter(|operation| !operation.is_empty())
            .map(ToString::to_string);
        let blocking_finding_count = u64::from(expected_should_block(
            &provider,
            evaluation.read_only,
            evaluation.mutation,
            evaluation.autopilot,
            evaluation.production_target,
            evaluation.signed_override_active,
        ));
        Self {
            identifier: identifier.into(),
            provider,
            operation,
            router_management: evaluation.router_management,
            read_only: evaluation.read_only,
            mutation: evaluation.mutation,
            autopilot: evaluation.autopilot,
            tool_input_present: evaluation.tool_input_present,
            production_target: evaluation.production_target,
            session_id_present: evaluation.session_id_present,
            signed_override_active: evaluation.signed_override_active,
            auth0_override_supported: evaluation.auth0_override_supported,
            blocking_finding_count,
            should_block: blocking_finding_count > 0,
            decision: DopplerAuth0Decision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DopplerAuth0GraphRun {
    pub state: DopplerAuth0State,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DopplerAuth0State>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct DopplerAuth0Authorization {
    decision: DopplerAuth0Decision,
    thread_id: String,
    checkpoint_id: String,
}

impl DopplerAuth0Authorization {
    #[must_use]
    pub fn decision(&self) -> DopplerAuth0Decision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl DopplerAuth0GraphRun {
    #[must_use]
    pub fn doppler_auth0_authorization(&self) -> Result<Option<DopplerAuth0Authorization>, String> {
        if self.state.decision == DopplerAuth0Decision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "doppler_auth0",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(DopplerAuth0Authorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW: &str = "allow";
const ALLOW_READ_ONLY: &str = "allow_read_only";
const ALLOW_AUTOPILOT_NONPROD: &str = "allow_autopilot_nonprod";
const ALLOW_SIGNED_OVERRIDE: &str = "allow_signed_override";
const BLOCK: &str = "block";

pub type DopplerAuth0Graph = CompilationResult<DopplerAuth0State>;

#[must_use]
pub const fn provider_label(provider: DopplerAuth0Provider) -> &'static str {
    match provider {
        DopplerAuth0Provider::None => "none",
        DopplerAuth0Provider::Doppler => "doppler",
        DopplerAuth0Provider::Auth0 => "auth0",
    }
}

#[must_use]
pub fn doppler_auth0_decision_label(decision: DopplerAuth0Decision) -> &'static str {
    match decision {
        DopplerAuth0Decision::Unclassified => "unclassified",
        DopplerAuth0Decision::Allow => "allow",
        DopplerAuth0Decision::AllowReadOnly => "allow-read-only",
        DopplerAuth0Decision::AllowAutopilotNonProd => "allow-autopilot-nonprod",
        DopplerAuth0Decision::AllowSignedOverride => "allow-signed-override",
        DopplerAuth0Decision::Block => "block",
    }
}

fn expected_should_block(
    provider: &str,
    read_only: bool,
    mutation: bool,
    autopilot: bool,
    production_target: bool,
    signed_override_active: bool,
) -> bool {
    let policy_block = match provider {
        "none" => false,
        "doppler" => mutation && !(autopilot && !production_target) && !signed_override_active,
        "auth0" => mutation && !(autopilot && !production_target),
        _ => true,
    };
    policy_block || (read_only && mutation)
}

fn expected_decision(state: &DopplerAuth0State) -> DopplerAuth0Decision {
    if state.should_block {
        DopplerAuth0Decision::Block
    } else if state.signed_override_active {
        DopplerAuth0Decision::AllowSignedOverride
    } else if state.autopilot && state.mutation && !state.production_target {
        DopplerAuth0Decision::AllowAutopilotNonProd
    } else if state.read_only {
        DopplerAuth0Decision::AllowReadOnly
    } else {
        DopplerAuth0Decision::Allow
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "doppler_auth0")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn doppler_auth0_state_schema() -> StateSchema<DopplerAuth0State> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "provider",
                "operation",
                "router_management",
                "read_only",
                "mutation",
                "autopilot",
                "tool_input_present",
                "production_target",
                "session_id_present",
                "signed_override_active",
                "auth0_override_supported",
                "blocking_finding_count",
                "should_block",
                "decision"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "provider": {
                    "type": "string",
                    "enum": ["none", "doppler", "auth0"]
                },
                "operation": {
                    "anyOf": [
                        { "type": "string", "minLength": 1 },
                        { "type": "null" }
                    ]
                },
                "router_management": { "type": "boolean" },
                "read_only": { "type": "boolean" },
                "mutation": { "type": "boolean" },
                "autopilot": { "type": "boolean" },
                "tool_input_present": { "type": "boolean" },
                "production_target": { "type": "boolean" },
                "session_id_present": { "type": "boolean" },
                "signed_override_active": { "type": "boolean" },
                "auth0_override_supported": { "type": "boolean" },
                "blocking_finding_count": { "type": "integer", "minimum": 0, "maximum": 1 },
                "should_block": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": [
                        "Unclassified",
                        "Allow",
                        "AllowReadOnly",
                        "AllowAutopilotNonProd",
                        "AllowSignedOverride",
                        "Block"
                    ]
                }
            },
            "x-sentinel": {
                "graph": "doppler_auth0",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &DopplerAuth0State| {
            if state.provider == "none"
                && (state.router_management
                    || state.operation.is_some()
                    || state.read_only
                    || state.mutation
                    || state.production_target
                    || state.signed_override_active
                    || state.auth0_override_supported
                    || state.blocking_finding_count > 0
                    || state.should_block)
            {
                return Err(StateError::ValidationFailed(
                    "doppler_auth0 non-provider state cannot carry authorization facts"
                        .to_string(),
                ));
            }
            if state.provider != "none"
                && state
                    .operation
                    .as_deref()
                    .map(str::trim)
                    .filter(|operation| !operation.is_empty())
                    .is_none()
            {
                return Err(StateError::ValidationFailed(
                    "doppler_auth0 provider state requires an operation".to_string(),
                ));
            }
            if state.read_only && state.mutation {
                return Err(StateError::ValidationFailed(
                    "doppler_auth0 read_only and mutation cannot both be true".to_string(),
                ));
            }
            if state.router_management && !state.read_only {
                return Err(StateError::ValidationFailed(
                    "doppler_auth0 router management operations must be read-only".to_string(),
                ));
            }
            let expected_mutation = match state.provider.as_str() {
                "none" => false,
                "doppler" | "auth0" => !state.read_only,
                _ => false,
            };
            if state.mutation != expected_mutation {
                return Err(StateError::ValidationFailed(format!(
                    "doppler_auth0 mutation must match provider/read-only facts: expected \
                     {expected_mutation}, got {}",
                    state.mutation
                )));
            }
            if state.provider == "auth0" && state.auth0_override_supported {
                return Err(StateError::ValidationFailed(
                    "doppler_auth0 Auth0 overrides are not supported".to_string(),
                ));
            }
            if state.signed_override_active {
                if state.provider != "doppler" || !state.mutation || !state.session_id_present {
                    return Err(StateError::ValidationFailed(
                        "doppler_auth0 signed override only applies to session-scoped Doppler mutations"
                            .to_string(),
                    ));
                }
            }
            let expected_should_block = expected_should_block(
                &state.provider,
                state.read_only,
                state.mutation,
                state.autopilot,
                state.production_target,
                state.signed_override_active,
            );
            if state.should_block != expected_should_block {
                return Err(StateError::ValidationFailed(format!(
                    "doppler_auth0 should_block must match provider/autopilot/override policy: \
                     expected {expected_should_block}, got {}",
                    state.should_block
                )));
            }
            let expected_blocking_finding_count = u64::from(expected_should_block);
            if state.blocking_finding_count != expected_blocking_finding_count {
                return Err(StateError::ValidationFailed(format!(
                    "doppler_auth0 blocking_finding_count must match should_block: expected \
                     {expected_blocking_finding_count}, got {}",
                    state.blocking_finding_count
                )));
            }
            if state.decision != DopplerAuth0Decision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "doppler_auth0 terminal decision must match derived authorization: terminal \
                     decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(state: DopplerAuth0State) -> Result<DopplerAuth0State, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_doppler_auth0_graph() -> Result<DopplerAuth0Graph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("doppler_auth0").await?;
    build_doppler_auth0_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_doppler_auth0_graph_with_ephemeral_sqlite() -> Result<DopplerAuth0Graph, String> {
    build_doppler_auth0_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_doppler_auth0_graph_with_database_path(
    db_path: &str,
) -> Result<DopplerAuth0Graph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_doppler_auth0_graph_with_checkpointer(checkpointer).await
}

async fn build_doppler_auth0_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<DopplerAuth0Graph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = doppler_auth0_state_schema();
    let builder = StateGraphBuilder::<DopplerAuth0State>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: DopplerAuth0State| async move {
                emit_decision_node_event("doppler_auth0", CLASSIFY, &s.identifier)?;
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
            |s: DopplerAuth0State| async move {
                emit_decision_node_event("doppler_auth0", ALLOW, &s.identifier)?;
                let mut next = s;
                next.decision = DopplerAuth0Decision::Allow;
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
            ALLOW_READ_ONLY,
            |s: DopplerAuth0State| async move {
                emit_decision_node_event("doppler_auth0", ALLOW_READ_ONLY, &s.identifier)?;
                let mut next = s;
                next.decision = DopplerAuth0Decision::AllowReadOnly;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_READ_ONLY,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW_AUTOPILOT_NONPROD,
            |s: DopplerAuth0State| async move {
                emit_decision_node_event("doppler_auth0", ALLOW_AUTOPILOT_NONPROD, &s.identifier)?;
                let mut next = s;
                next.decision = DopplerAuth0Decision::AllowAutopilotNonProd;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_AUTOPILOT_NONPROD,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            ALLOW_SIGNED_OVERRIDE,
            |s: DopplerAuth0State| async move {
                emit_decision_node_event("doppler_auth0", ALLOW_SIGNED_OVERRIDE, &s.identifier)?;
                let mut next = s;
                next.decision = DopplerAuth0Decision::AllowSignedOverride;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_SIGNED_OVERRIDE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            BLOCK,
            |s: DopplerAuth0State| async move {
                emit_decision_node_event("doppler_auth0", BLOCK, &s.identifier)?;
                let mut next = s;
                next.decision = DopplerAuth0Decision::Block;
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
        .add_conditional_edge(CLASSIFY, |s: &DopplerAuth0State| {
            match expected_decision(s) {
                DopplerAuth0Decision::Allow => ALLOW.into(),
                DopplerAuth0Decision::AllowReadOnly => ALLOW_READ_ONLY.into(),
                DopplerAuth0Decision::AllowAutopilotNonProd => ALLOW_AUTOPILOT_NONPROD.into(),
                DopplerAuth0Decision::AllowSignedOverride => ALLOW_SIGNED_OVERRIDE.into(),
                DopplerAuth0Decision::Block => BLOCK.into(),
                DopplerAuth0Decision::Unclassified => ALLOW.into(),
            }
        })
        .add_edge(ALLOW, END)
        .add_edge(ALLOW_READ_ONLY, END)
        .add_edge(ALLOW_AUTOPILOT_NONPROD, END)
        .add_edge(ALLOW_SIGNED_OVERRIDE, END)
        .add_edge(BLOCK, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_doppler_auth0_decision_report(
    compiled: &DopplerAuth0Graph,
    state: DopplerAuth0State,
) -> Result<DopplerAuth0GraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "doppler_auth0",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed =
        stream_decision_run(compiled, &thread_id, "doppler_auth0", &identifier, state).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "doppler_auth0",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(DopplerAuth0GraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: doppler_auth0_graph_topology(compiled)?,
    })
}

pub fn doppler_auth0_graph_topology(
    compiled: &DopplerAuth0Graph,
) -> Result<DecisionGraphTopology, String> {
    topology("doppler_auth0", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::doppler_auth0_gate::{
        DopplerAuth0Decision as AppDecision, DopplerAuth0Evaluation,
    };

    fn evaluation(provider: DopplerAuth0Provider, operation: &str) -> DopplerAuth0Evaluation {
        DopplerAuth0Evaluation {
            tool: Some(format!("mcp__{}__{operation}", provider_label(provider))),
            operation: Some(operation.to_string()),
            provider,
            router_management: false,
            read_only: false,
            mutation: true,
            autopilot: false,
            tool_input_present: true,
            production_target: true,
            session_id_present: true,
            signed_override_active: false,
            auth0_override_supported: false,
            should_block: true,
            decision: AppDecision::Block,
            block_reason: Some("blocked".to_string()),
        }
    }

    #[tokio::test]
    async fn graph_authorizes_doppler_prod_block() {
        let graph = build_doppler_auth0_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = DopplerAuth0State::from_evaluation(
            "doppler-prod",
            &evaluation(DopplerAuth0Provider::Doppler, "set_secret"),
        );
        let run = run_doppler_auth0_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, DopplerAuth0Decision::Block);
        assert!(run
            .doppler_auth0_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_auth0_autopilot_nonprod_allow() {
        let graph = build_doppler_auth0_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(DopplerAuth0Provider::Auth0, "update_rule");
        eval.autopilot = true;
        eval.production_target = false;
        eval.should_block = false;
        eval.decision = AppDecision::AllowAutopilotNonProd;
        let state = DopplerAuth0State::from_evaluation("auth0-dev", &eval);
        let run = run_doppler_auth0_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            DopplerAuth0Decision::AllowAutopilotNonProd
        );
    }

    #[tokio::test]
    async fn graph_authorizes_signed_doppler_override_allow() {
        let graph = build_doppler_auth0_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = evaluation(DopplerAuth0Provider::Doppler, "set_secret");
        eval.signed_override_active = true;
        eval.should_block = false;
        eval.decision = AppDecision::AllowSignedOverride;
        let state = DopplerAuth0State::from_evaluation("doppler-override", &eval);
        let run = run_doppler_auth0_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            DopplerAuth0Decision::AllowSignedOverride
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_auth0_override() {
        let graph = build_doppler_auth0_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = DopplerAuth0State::from_evaluation(
            "auth0-forged",
            &evaluation(DopplerAuth0Provider::Auth0, "update_user"),
        );
        state.signed_override_active = true;
        state.should_block = false;
        state.blocking_finding_count = 0;
        let err = run_doppler_auth0_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("signed override"), "{err}");
    }
}
