//! LangGraph batch orchestration for audit commands.
//!
//! Per-item decision graphs remain the authority for each external decision.
//! This graph owns the batch-level map/reduce shape: it fans audit records out
//! with `NodeSend`, merges worker results through dynamic channel reducers, and
//! joins through a deferred LangGraph node before the CLI writes JSONL rows.

use std::time::Duration;

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::services::FunctionReducer;
use langgraph_core::domain::value_objects::{
    DynamicState, EdgeResult, NodeConfig, NodeError, NodeSend, NodeTimeoutPolicy, StateError,
    StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

use crate::decision_graph_introspection::{
    checkpoint_history, emit_decision_node_event, stream_decision_run,
    terminal_decision_checkpoint_result, topology, validate_decision_graph_run,
    DecisionGraphCheckpointSnapshot, DecisionGraphStreamPart, DecisionGraphTopology,
    DecisionGraphWriteHistoryEntry,
};

const DISPATCH: &str = "dispatch";
const PLAN_ITEM: &str = "plan_item";
const JOIN: &str = "join";
const BATCH_IDENTIFIER: &str = "batch";

pub type BatchAuditGraph = CompilationResult<DynamicState>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchAuditItem {
    pub identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

impl BatchAuditItem {
    #[must_use]
    pub fn new(identifier: impl Into<String>, category: Option<String>) -> Self {
        Self {
            identifier: identifier.into(),
            category,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchAuditPlannedItem {
    pub ordinal: usize,
    pub identifier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchAuditGraphRun {
    pub workflow_authority: &'static str,
    pub graph: String,
    pub target_graph: String,
    pub thread_id: String,
    pub items_requested: usize,
    pub items_dispatched: usize,
    pub planned_items: Vec<BatchAuditPlannedItem>,
    pub checkpoints: Vec<DecisionGraphCheckpointSnapshot<DynamicState>>,
    pub write_history: Vec<DecisionGraphWriteHistoryEntry>,
    pub stream: Vec<DecisionGraphStreamPart>,
    pub topology: DecisionGraphTopology,
}

fn node_config(
    graph_name: &str,
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", graph_name)
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_metadata(
            "sentinel.checkpointer_tenant_scope",
            checkpointer_tenant_scope,
        )
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn deferred_node_config(
    graph_name: &str,
    node: &str,
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: &str,
) -> NodeConfig {
    node_config(
        graph_name,
        node,
        checkpointer_backend,
        checkpointer_scope,
        checkpointer_tenant_scope,
    )
    .with_defer(true)
}

fn batch_audit_state_schema(graph_name: &str) -> StateSchema<DynamicState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_required_channel("workflow_authority")
        .with_required_channel("identifier")
        .with_required_channel("target_graph")
        .with_required_channel("items")
        .with_required_channel("planned_items")
        .with_required_channel("items_dispatched")
        .with_required_channel("batch_complete")
        .with_optional_channel("fanout_planned")
        .with_optional_channel("item_identifier")
        .with_optional_channel("category")
        .with_optional_channel("ordinal")
        .with_optional_channel("empty_batch_probe")
        .with_channel_validator("workflow_authority", |value| match value.as_str() {
            Some("langgraph") => Ok(()),
            _ => Err(StateError::ValidationFailed(
                "batch audit workflow_authority must be langgraph".to_string(),
            )),
        })
        .with_channel_validator("identifier", non_empty_string("batch audit identifier"))
        .with_channel_validator("target_graph", non_empty_string("batch audit target_graph"))
        .with_channel_validator("items", array_channel("batch audit items"))
        .with_channel_validator("planned_items", array_channel("batch audit planned_items"))
        .with_channel_validator("items_dispatched", |value| {
            if value.as_u64().is_some() {
                Ok(())
            } else {
                Err(StateError::ValidationFailed(
                    "batch audit items_dispatched must be an unsigned integer".to_string(),
                ))
            }
        })
        .with_channel_validator("batch_complete", |value| {
            if value.as_bool().is_some() {
                Ok(())
            } else {
                Err(StateError::ValidationFailed(
                    "batch audit batch_complete must be boolean".to_string(),
                ))
            }
        })
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": true,
            "required": [
                "workflow_authority",
                "identifier",
                "target_graph",
                "items",
                "planned_items",
                "items_dispatched",
                "batch_complete"
            ],
            "properties": {
                "workflow_authority": { "type": "string", "const": "langgraph" },
                "identifier": { "type": "string", "const": "batch" },
                "target_graph": { "type": "string", "minLength": 1 },
                "items": { "type": "array" },
                "planned_items": { "type": "array" },
                "items_dispatched": { "type": "integer", "minimum": 0 },
                "batch_complete": { "type": "boolean" }
            },
            "x-sentinel": {
                "graph": graph_name,
                "authority": "langgraph"
            }
        }))
}

fn non_empty_string(
    label: &'static str,
) -> impl Fn(&Value) -> Result<(), StateError> + Send + Sync + 'static {
    move |value| match value.as_str() {
        Some(value) if !value.trim().is_empty() => Ok(()),
        _ => Err(StateError::ValidationFailed(format!(
            "{label} must be a non-empty string"
        ))),
    }
}

fn array_channel(
    label: &'static str,
) -> impl Fn(&Value) -> Result<(), StateError> + Send + Sync + 'static {
    move |value| {
        if value.as_array().is_some() {
            Ok(())
        } else {
            Err(StateError::ValidationFailed(format!(
                "{label} must be an array"
            )))
        }
    }
}

fn append_json_arrays(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Array(mut left_items), Value::Array(right_items)) => {
            left_items.extend(right_items);
            Value::Array(left_items)
        }
        (_, right_value) => right_value,
    }
}

fn sum_json_u64(left: Value, right: Value) -> Value {
    Value::from(left.as_u64().unwrap_or_default() + right.as_u64().unwrap_or_default())
}

fn keep_left_json(left: Value, _right: Value) -> Value {
    left
}

fn state_get<T>(state: &DynamicState, channel: &str) -> Result<T, NodeError>
where
    T: DeserializeOwned,
{
    state
        .get(channel)
        .map_err(|err| NodeError::InvalidState(err.to_string()))
}

fn state_set<T>(state: &mut DynamicState, channel: &str, value: T) -> Result<(), NodeError>
where
    T: Serialize,
{
    state
        .set(channel, value)
        .map_err(|err| NodeError::InvalidState(err.to_string()))
}

fn build_initial_state(
    target_graph: &str,
    items: &[BatchAuditItem],
) -> Result<DynamicState, String> {
    let mut state = DynamicState::new();
    state
        .set("workflow_authority", "langgraph")
        .map_err(|err| err.to_string())?;
    state
        .set("identifier", BATCH_IDENTIFIER)
        .map_err(|err| err.to_string())?;
    state
        .set("target_graph", target_graph)
        .map_err(|err| err.to_string())?;
    state.set("items", items).map_err(|err| err.to_string())?;
    state
        .set("planned_items", Vec::<BatchAuditPlannedItem>::new())
        .map_err(|err| err.to_string())?;
    state
        .set("items_dispatched", 0_u64)
        .map_err(|err| err.to_string())?;
    state
        .set("batch_complete", false)
        .map_err(|err| err.to_string())?;
    state
        .set("item_identifier", "")
        .map_err(|err| err.to_string())?;
    state
        .set("category", Option::<String>::None)
        .map_err(|err| err.to_string())?;
    state
        .set("ordinal", 0_usize)
        .map_err(|err| err.to_string())?;
    state
        .set("empty_batch_probe", false)
        .map_err(|err| err.to_string())?;
    Ok(state)
}

pub async fn build_batch_audit_graph(graph_name: &str) -> Result<BatchAuditGraph, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph(graph_name).await?;
    build_batch_audit_graph_with_checkpointer(graph_name, checkpointer).await
}

async fn build_batch_audit_graph_with_checkpointer(
    graph_name: &str,
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<BatchAuditGraph, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let checkpointer_tenant_scope = checkpointer.tenant_scope_metadata_value();
    let schema = batch_audit_state_schema(graph_name);
    let dispatch_graph = graph_name.to_string();
    let plan_graph = graph_name.to_string();
    let join_graph = graph_name.to_string();

    let builder = StateGraphBuilder::<DynamicState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema.clone())
        .with_context_schema(schema)
        .set_channel_reducer("planned_items", FunctionReducer::new(append_json_arrays))
        .set_channel_reducer("items_dispatched", FunctionReducer::new(sum_json_u64))
        .set_channel_reducer("item_identifier", FunctionReducer::new(keep_left_json))
        .set_channel_reducer("category", FunctionReducer::new(keep_left_json))
        .set_channel_reducer("ordinal", FunctionReducer::new(keep_left_json))
        .set_channel_reducer("empty_batch_probe", FunctionReducer::new(keep_left_json))
        .add_node_with_config_and_error_handler(
            DISPATCH,
            move |state: &DynamicState| {
                let batch_id: String = state_get(state, "identifier")?;
                emit_decision_node_event(&dispatch_graph, DISPATCH, &batch_id)?;
                let mut next = state.clone();
                state_set(&mut next, "fanout_planned", true)?;
                Ok(next)
            },
            node_config(
                graph_name,
                DISPATCH,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_node_with_config_and_error_handler(
            PLAN_ITEM,
            move |state: &DynamicState| {
                let batch_id: String = state_get(state, "identifier")?;
                emit_decision_node_event(&plan_graph, PLAN_ITEM, &batch_id)?;
                let ordinal: usize = state_get(state, "ordinal")?;
                let empty_batch_probe = state
                    .get::<bool>("empty_batch_probe")
                    .map_err(|err| NodeError::InvalidState(err.to_string()))
                    .unwrap_or(false);
                if empty_batch_probe {
                    let mut next = state.clone();
                    state_set(
                        &mut next,
                        "planned_items",
                        Vec::<BatchAuditPlannedItem>::new(),
                    )?;
                    state_set(&mut next, "items_dispatched", 0_u64)?;
                    return Ok(next);
                }
                let identifier: String = state_get(state, "item_identifier")?;
                let category: Option<String> = state_get(state, "category")?;
                let planned = BatchAuditPlannedItem {
                    ordinal,
                    identifier,
                    category,
                };
                let mut next = state.clone();
                state_set(&mut next, "planned_items", vec![planned])?;
                state_set(&mut next, "items_dispatched", 1_u64)?;
                Ok(next)
            },
            node_config(
                graph_name,
                PLAN_ITEM,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_node_with_config_and_error_handler(
            JOIN,
            move |state: &DynamicState| {
                let batch_id: String = state_get(state, "identifier")?;
                emit_decision_node_event(&join_graph, JOIN, &batch_id)?;
                let mut next = state.clone();
                state_set(&mut next, "batch_complete", true)?;
                Ok(next)
            },
            deferred_node_config(
                graph_name,
                JOIN,
                checkpointer_backend,
                checkpointer_scope,
                checkpointer_tenant_scope,
            ),
            crate::decision_graph_introspection::decision_node_error_handler,
        )
        .add_edge(START, DISPATCH)
        .add_dynamic_edge(DISPATCH, |state: &DynamicState| {
            let Ok(items) = state.get::<Vec<BatchAuditItem>>("items") else {
                return EdgeResult::single(JOIN);
            };
            if items.is_empty() {
                let mut send_state = state.clone();
                let _ = send_state.set("ordinal", 0_usize);
                let _ = send_state.set("empty_batch_probe", true);
                return EdgeResult::multiple(vec![NodeSend::new(PLAN_ITEM, send_state)]);
            }

            EdgeResult::multiple(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(ordinal, item)| {
                        let mut send_state = state.clone();
                        let _ = send_state.set("ordinal", ordinal);
                        let _ = send_state.set("item_identifier", item.identifier);
                        let _ = send_state.set("category", item.category);
                        let priority =
                            i32::MAX.saturating_sub(i32::try_from(ordinal).unwrap_or(i32::MAX));
                        NodeSend::new(PLAN_ITEM, send_state).with_priority(priority)
                    })
                    .collect(),
            )
        })
        .add_edge(PLAN_ITEM, JOIN)
        .add_edge(JOIN, END);

    let graph = builder.build().map_err(|err| err.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|err| err.to_string())
}

pub async fn run_batch_audit_graph_report(
    graph_name: &str,
    target_graph: &str,
    items: &[BatchAuditItem],
) -> Result<BatchAuditGraphRun, String> {
    let compiled = build_batch_audit_graph(graph_name).await?;
    run_batch_audit_graph_report_with_compiled(&compiled, graph_name, target_graph, items).await
}

async fn run_batch_audit_graph_report_with_compiled(
    compiled: &BatchAuditGraph,
    graph_name: &str,
    target_graph: &str,
    items: &[BatchAuditItem],
) -> Result<BatchAuditGraphRun, String> {
    let input = build_initial_state(target_graph, items)?;
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        graph_name,
        BATCH_IDENTIFIER,
        &input,
    )?;
    let streamed =
        stream_decision_run(compiled, &thread_id, graph_name, BATCH_IDENTIFIER, input).await?;
    let checkpoints = checkpoint_history(compiled, &thread_id).await?;
    let write_history =
        crate::decision_graph_introspection::write_history(compiled, &thread_id, None).await?;
    validate_decision_graph_run(
        graph_name,
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    let _terminal_checkpoint = terminal_decision_checkpoint_result(
        graph_name,
        &thread_id,
        &streamed.state,
        &checkpoints,
        &write_history,
    )?;

    let items_dispatched = streamed
        .state
        .get::<u64>("items_dispatched")
        .map_err(|err| err.to_string())? as usize;
    let mut planned_items = streamed
        .state
        .get::<Vec<BatchAuditPlannedItem>>("planned_items")
        .map_err(|err| err.to_string())?;
    planned_items.sort_by(|left, right| {
        left.ordinal
            .cmp(&right.ordinal)
            .then_with(|| left.identifier.cmp(&right.identifier))
    });

    if items_dispatched != items.len() || planned_items.len() != items.len() {
        return Err(format!(
            "{graph_name} dispatched {} items and planned {}, expected {}",
            items_dispatched,
            planned_items.len(),
            items.len()
        ));
    }

    Ok(BatchAuditGraphRun {
        workflow_authority: "langgraph",
        graph: graph_name.to_string(),
        target_graph: target_graph.to_string(),
        thread_id,
        items_requested: items.len(),
        items_dispatched,
        planned_items,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: topology(graph_name, compiled)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ephemeral_batch_graph() -> BatchAuditGraph {
        let checkpointer = crate::decision_graph_store::checkpointer_for_config(
            crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
                database_path: ":memory:".to_string(),
            },
        )
        .await
        .expect("checkpointer");
        build_batch_audit_graph_with_checkpointer("pm_audit_batch", checkpointer)
            .await
            .expect("batch graph")
    }

    #[tokio::test]
    async fn batch_graph_fans_out_with_sends_and_joins_with_reducers() {
        let graph = ephemeral_batch_graph().await;
        let items = vec![
            BatchAuditItem::new("FPCRM-901", Some("oversized".to_string())),
            BatchAuditItem::new("FPCRM-902", Some("qa-failed".to_string())),
        ];

        let run = run_batch_audit_graph_report_with_compiled(
            &graph,
            "pm_audit_batch",
            "pm_audit",
            &items,
        )
        .await
        .expect("batch graph run");

        assert_eq!(run.workflow_authority, "langgraph");
        assert_eq!(run.graph, "pm_audit_batch");
        assert_eq!(run.target_graph, "pm_audit");
        assert_eq!(run.items_requested, 2);
        assert_eq!(run.items_dispatched, 2);
        assert_eq!(run.planned_items[0].identifier, "FPCRM-901");
        assert_eq!(run.planned_items[1].identifier, "FPCRM-902");
        assert!(run.topology.edges.iter().any(|edge| edge.kind == "dynamic"));
        assert!(run
            .topology
            .nodes
            .iter()
            .any(|node| node.id == JOIN && node.deferred));
        assert!(run.write_history.iter().any(|write| {
            write.channel == "state" && write.value_json["channels"]["batch_complete"] == true
        }));
        assert!(run.stream.iter().any(|part| {
            part.payload_kind == "custom" && part.payload_json["node_id"] == PLAN_ITEM
        }));
    }

    #[tokio::test]
    async fn batch_graph_accepts_empty_batches() {
        let graph = ephemeral_batch_graph().await;
        let run =
            run_batch_audit_graph_report_with_compiled(&graph, "pm_audit_batch", "pm_audit", &[])
                .await
                .expect("empty batch graph run");

        assert_eq!(run.items_requested, 0);
        assert_eq!(run.items_dispatched, 0);
        assert!(run.planned_items.is_empty());
        assert!(run
            .stream
            .iter()
            .any(|part| { part.payload_kind == "custom" && part.payload_json["node_id"] == JOIN }));
    }
}
