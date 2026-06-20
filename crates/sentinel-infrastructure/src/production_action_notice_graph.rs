//! Graph-backed production action notice authorization.
//!
//! When a production override is armed, mutating production-touching actions
//! must surface a dual-display notice. This graph authorizes the notice/silent
//! decision through durable LangGraph checkpoints before the CLI proceeds.

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use sentinel_application::hooks::production_action_notice::ProductionActionNoticeEvaluation;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run, write_history,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProductionActionNoticeDecision {
    #[default]
    Unclassified,
    AllowSilent,
    Notice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProductionActionNoticeState {
    pub identifier: String,
    pub tool: Option<String>,
    pub production_override_armed: bool,
    pub tool_present: bool,
    pub pure_read: bool,
    pub mutating_tool: bool,
    pub tool_input_present: bool,
    pub file_path_present: bool,
    pub haystack_present: bool,
    pub haystack_sha256: Option<String>,
    pub mentions_prod: bool,
    pub should_notice: bool,
    pub decision: ProductionActionNoticeDecision,
}

impl ProductionActionNoticeState {
    #[must_use]
    pub fn from_evaluation(
        identifier: impl Into<String>,
        evaluation: &ProductionActionNoticeEvaluation,
    ) -> Self {
        let haystack_sha256 = evaluation
            .haystack_present
            .then(|| sha256(&evaluation.haystack));
        Self {
            identifier: identifier.into(),
            tool: evaluation.tool.clone(),
            production_override_armed: evaluation.production_override_armed,
            tool_present: evaluation.tool_present,
            pure_read: evaluation.pure_read,
            mutating_tool: evaluation.mutating_tool,
            tool_input_present: evaluation.tool_input_present,
            file_path_present: evaluation.file_path_present,
            haystack_present: evaluation.haystack_present,
            haystack_sha256,
            mentions_prod: evaluation.mentions_prod,
            should_notice: evaluation.should_notice,
            decision: ProductionActionNoticeDecision::Unclassified,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductionActionNoticeGraphRun {
    pub state: ProductionActionNoticeState,
    pub thread_id: String,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<ProductionActionNoticeState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

#[derive(Debug, Clone)]
pub struct ProductionActionNoticeAuthorization {
    decision: ProductionActionNoticeDecision,
    thread_id: String,
    checkpoint_id: String,
}

impl ProductionActionNoticeAuthorization {
    #[must_use]
    pub fn decision(&self) -> ProductionActionNoticeDecision {
        self.decision
    }

    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

impl ProductionActionNoticeGraphRun {
    #[must_use]
    pub fn production_action_notice_authorization(
        &self,
    ) -> Result<Option<ProductionActionNoticeAuthorization>, String> {
        if self.state.decision == ProductionActionNoticeDecision::Unclassified {
            return Ok(None);
        }
        let checkpoint_id = terminal_decision_checkpoint_result(
            "production_action_notice",
            &self.thread_id,
            &self.state,
            &self.checkpoints,
            &self.write_history,
        )?
        .checkpoint_id
        .clone();
        Ok(Some(ProductionActionNoticeAuthorization {
            decision: self.state.decision,
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }
}

const CLASSIFY: &str = "classify";
const ALLOW_SILENT: &str = "allow_silent";
const NOTICE: &str = "notice";

pub type ProductionActionNoticeGraph = CompilationResult<ProductionActionNoticeState>;

#[must_use]
pub fn production_action_notice_decision_label(
    decision: ProductionActionNoticeDecision,
) -> &'static str {
    match decision {
        ProductionActionNoticeDecision::Unclassified => "unclassified",
        ProductionActionNoticeDecision::AllowSilent => "allow-silent",
        ProductionActionNoticeDecision::Notice => "notice",
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

fn expected_should_notice(state: &ProductionActionNoticeState) -> bool {
    state.production_override_armed
        && state.tool_present
        && state.mutating_tool
        && !state.pure_read
        && state.mentions_prod
}

fn expected_decision(state: &ProductionActionNoticeState) -> ProductionActionNoticeDecision {
    if state.should_notice {
        ProductionActionNoticeDecision::Notice
    } else {
        ProductionActionNoticeDecision::AllowSilent
    }
}

fn node_config(
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "production_action_notice")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
}

fn production_action_notice_state_schema() -> StateSchema<ProductionActionNoticeState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "tool",
                "production_override_armed",
                "tool_present",
                "pure_read",
                "mutating_tool",
                "tool_input_present",
                "file_path_present",
                "haystack_present",
                "haystack_sha256",
                "mentions_prod",
                "should_notice",
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
                "production_override_armed": { "type": "boolean" },
                "tool_present": { "type": "boolean" },
                "pure_read": { "type": "boolean" },
                "mutating_tool": { "type": "boolean" },
                "tool_input_present": { "type": "boolean" },
                "file_path_present": { "type": "boolean" },
                "haystack_present": { "type": "boolean" },
                "haystack_sha256": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                },
                "mentions_prod": { "type": "boolean" },
                "should_notice": { "type": "boolean" },
                "decision": {
                    "type": "string",
                    "enum": ["Unclassified", "AllowSilent", "Notice"]
                }
            },
            "x-sentinel": {
                "graph": "production_action_notice",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &ProductionActionNoticeState| {
            if state
                .tool
                .as_deref()
                .is_none_or(|tool| tool.trim().is_empty())
            {
                return Err(StateError::ValidationFailed(
                    "LangGraph tool-authority state requires concrete tool identity".to_string(),
                ));
            }
            if !state.tool_present
                && (state.pure_read
                    || state.mutating_tool
                    || state.haystack_present
                    || state.haystack_sha256.is_some()
                    || state.mentions_prod
                    || state.should_notice)
            {
                return Err(StateError::ValidationFailed(
                    "production_action_notice missing tool cannot carry action facts".to_string(),
                ));
            }
            if !state.production_override_armed
                && (state.haystack_present
                    || state.haystack_sha256.is_some()
                    || state.mentions_prod
                    || state.should_notice)
            {
                return Err(StateError::ValidationFailed(
                    "production_action_notice disarmed state cannot carry production notice facts"
                        .to_string(),
                ));
            }
            if state.pure_read && state.mutating_tool {
                return Err(StateError::ValidationFailed(
                    "production_action_notice tool cannot be pure read and mutating".to_string(),
                ));
            }
            if state.haystack_present {
                if !state.mutating_tool || state.pure_read {
                    return Err(StateError::ValidationFailed(
                        "production_action_notice haystack scan requires mutating non-read tool"
                            .to_string(),
                    ));
                }
                if !optional_hex_digest_present(&state.haystack_sha256) {
                    return Err(StateError::ValidationFailed(
                        "production_action_notice haystack_sha256 must be a 64-character hex digest"
                            .to_string(),
                    ));
                }
            } else if state.haystack_sha256.is_some() || state.mentions_prod {
                return Err(StateError::ValidationFailed(
                    "production_action_notice missing haystack cannot carry prod mention facts"
                        .to_string(),
                ));
            }
            let expected_should_notice = expected_should_notice(state);
            if state.should_notice != expected_should_notice {
                return Err(StateError::ValidationFailed(format!(
                    "production_action_notice should_notice must match prod action policy: expected \
                     {expected_should_notice}, got {}",
                    state.should_notice
                )));
            }
            if state.decision != ProductionActionNoticeDecision::Unclassified
                && state.decision != expected_decision(state)
            {
                return Err(StateError::ValidationFailed(format!(
                    "production_action_notice terminal decision must match derived authorization: \
                     terminal decision {:?} does not match expected {:?}",
                    state.decision,
                    expected_decision(state)
                )));
            }
            Ok(())
        })
}

async fn classify_node(
    state: ProductionActionNoticeState,
) -> Result<ProductionActionNoticeState, NodeError> {
    let mut next = state;
    next.decision = expected_decision(&next);
    Ok(next)
}

pub async fn build_production_action_notice_graph() -> Result<ProductionActionNoticeGraph, String> {
    let checkpointer =
        crate::decision_graph_store::checkpointer_for_graph("production_action_notice").await?;
    build_production_action_notice_graph_with_checkpointer(checkpointer).await
}

#[cfg(test)]
async fn build_production_action_notice_graph_with_ephemeral_sqlite(
) -> Result<ProductionActionNoticeGraph, String> {
    build_production_action_notice_graph_with_database_path(":memory:").await
}

#[cfg(test)]
async fn build_production_action_notice_graph_with_database_path(
    db_path: &str,
) -> Result<ProductionActionNoticeGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: db_path.to_string(),
        },
    )
    .await?;
    build_production_action_notice_graph_with_checkpointer(checkpointer).await
}

async fn build_production_action_notice_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<ProductionActionNoticeGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = production_action_notice_state_schema();
    let builder = StateGraphBuilder::<ProductionActionNoticeState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_node_defaults(crate::decision_graph_introspection::decision_node_defaults())
        .add_async_node_with_config_and_error_handler(
            CLASSIFY,
            |s: ProductionActionNoticeState| async move {
                emit_decision_node_event("production_action_notice", CLASSIFY, &s.identifier)?;
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
            ALLOW_SILENT,
            |s: ProductionActionNoticeState| async move {
                emit_decision_node_event("production_action_notice", ALLOW_SILENT, &s.identifier)?;
                let mut next = s;
                next.decision = ProductionActionNoticeDecision::AllowSilent;
                Ok::<_, NodeError>(next)
            },
            node_config(
                ALLOW_SILENT,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_async_node_with_config_and_error_handler(
            NOTICE,
            |s: ProductionActionNoticeState| async move {
                emit_decision_node_event("production_action_notice", NOTICE, &s.identifier)?;
                let mut next = s;
                next.decision = ProductionActionNoticeDecision::Notice;
                Ok::<_, NodeError>(next)
            },
            node_config(
                NOTICE,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, CLASSIFY)
        .add_conditional_edge(
            CLASSIFY,
            |s: &ProductionActionNoticeState| match expected_decision(s) {
                ProductionActionNoticeDecision::AllowSilent => ALLOW_SILENT.into(),
                ProductionActionNoticeDecision::Notice => NOTICE.into(),
                ProductionActionNoticeDecision::Unclassified => ALLOW_SILENT.into(),
            },
        )
        .add_edge(ALLOW_SILENT, END)
        .add_edge(NOTICE, END);
    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

pub async fn run_production_action_notice_decision_report(
    compiled: &ProductionActionNoticeGraph,
    state: ProductionActionNoticeState,
) -> Result<ProductionActionNoticeGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "production_action_notice",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = stream_decision_run(
        compiled,
        &thread_id,
        "production_action_notice",
        &identifier,
        state,
    )
    .await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history = write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        "production_action_notice",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(ProductionActionNoticeGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: production_action_notice_graph_topology(compiled)?,
    })
}

pub fn production_action_notice_graph_topology(
    compiled: &ProductionActionNoticeGraph,
) -> Result<DecisionGraphTopology, String> {
    topology("production_action_notice", compiled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_application::hooks::production_action_notice::{
        ProductionActionNoticeDecision as AppDecision, ProductionActionNoticeEvaluation,
    };

    fn notice_evaluation() -> ProductionActionNoticeEvaluation {
        ProductionActionNoticeEvaluation {
            production_override_armed: true,
            tool: Some("Bash".to_string()),
            tool_present: true,
            pure_read: false,
            mutating_tool: true,
            tool_input_present: true,
            file_path_present: false,
            haystack_present: true,
            haystack: r#"{"command":"wrangler deploy --env production"}"#.to_string(),
            mentions_prod: true,
            should_notice: true,
            decision: AppDecision::Notice,
        }
    }

    fn silent_evaluation() -> ProductionActionNoticeEvaluation {
        ProductionActionNoticeEvaluation {
            haystack: r#"{"command":"cargo test"}"#.to_string(),
            mentions_prod: false,
            should_notice: false,
            decision: AppDecision::AllowSilent,
            ..notice_evaluation()
        }
    }

    fn no_haystack_evaluation() -> ProductionActionNoticeEvaluation {
        ProductionActionNoticeEvaluation {
            haystack_present: false,
            haystack: String::new(),
            mentions_prod: false,
            should_notice: false,
            decision: AppDecision::AllowSilent,
            ..notice_evaluation()
        }
    }

    #[tokio::test]
    async fn graph_authorizes_notice() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProductionActionNoticeState::from_evaluation(
            "prod-action-notice",
            &notice_evaluation(),
        );
        assert_eq!(state.tool.as_deref(), Some("Bash"));
        assert!(optional_hex_digest_present(&state.haystack_sha256));
        let run = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(run.state.decision, ProductionActionNoticeDecision::Notice);
        assert!(run
            .production_action_notice_authorization()
            .unwrap()
            .unwrap()
            .checkpoint_ref()
            .contains('#'));
    }

    #[tokio::test]
    async fn graph_authorizes_silent_mutation() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProductionActionNoticeState::from_evaluation(
            "prod-action-silent",
            &silent_evaluation(),
        );
        let run = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            ProductionActionNoticeDecision::AllowSilent
        );
    }

    #[tokio::test]
    async fn graph_authorizes_missing_haystack_with_absent_hash() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let state = ProductionActionNoticeState::from_evaluation(
            "prod-action-no-haystack",
            &no_haystack_evaluation(),
        );
        assert_eq!(state.haystack_sha256, None);
        let run = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap();
        assert_eq!(
            run.state.decision,
            ProductionActionNoticeDecision::AllowSilent
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_silent_prod_action() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = ProductionActionNoticeState::from_evaluation(
            "prod-action-forged",
            &notice_evaluation(),
        );
        state.should_notice = false;
        let err = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("should_notice"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_tool_identity() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut eval = notice_evaluation();
        eval.tool = None;
        let state = ProductionActionNoticeState::from_evaluation("prod-action-missing-tool", &eval);
        let err = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("tool identity"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_present_haystack_without_digest() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = ProductionActionNoticeState::from_evaluation(
            "prod-action-missing-digest",
            &notice_evaluation(),
        );
        state.haystack_sha256 = None;
        let err = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("haystack_sha256"), "{err}");
    }

    #[tokio::test]
    async fn graph_schema_rejects_missing_haystack_with_extra_digest() {
        let graph = build_production_action_notice_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let mut state = ProductionActionNoticeState::from_evaluation(
            "prod-action-extra-digest",
            &no_haystack_evaluation(),
        );
        state.haystack_sha256 = Some(sha256(r#"{"command":"wrangler deploy --env production"}"#));
        let err = run_production_action_notice_decision_report(&graph, state)
            .await
            .unwrap_err();
        assert!(err.contains("missing haystack"), "{err}");
    }
}
