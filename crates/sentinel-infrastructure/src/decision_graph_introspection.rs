//! Runtime topology introspection for Sentinel's infrastructure decision graphs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use chrono::Utc;
use futures::StreamExt;
use langgraph_core::application::services::{
    get_stream_writer, ChatModelStream, CheckpointEvent, CheckpointsTransformer, CompilationResult,
    CompiledGraphV3Ext, CustomEvent, CustomTransformer, DebugEvent, DebugTransformer,
    GraphRunStream, LifecycleEvent, Projection, RunV3Options, StateSnapshot, StateUpdateEvent,
    SubgraphHandle, TaskEvent, TasksTransformer, UpdatesTransformer, V3StreamTransformer,
};
use langgraph_core::domain::value_objects::{NodeError, END, START};
use langgraph_core::ports::WriteHistoryEntry;
use sentinel_domain::langgraph_thread::validate_tenant_scope;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

const DECISION_STREAM_PROTOCOL: &str = "v3";
const STATE_SNAPSHOT_NODE: &str = "__state__";

/// Serializable view of one checkpoint write recorded by LangGraph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionGraphCheckpointWriteInfo {
    pub node_id: String,
    pub channel: String,
    pub ts: String,
}

/// Per-write history entry from LangGraph's checkpoint write stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionGraphWriteHistoryEntry {
    pub thread_id: String,
    pub checkpoint_id: String,
    pub step_number: u64,
    pub channel: String,
    pub node_id: String,
    pub ts: String,
    pub value_len: usize,
    pub value_sha256: String,
    pub value_json: serde_json::Value,
}

/// Serializable view of one LangGraph stream part emitted during a decision run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionGraphStreamPart {
    pub stream_protocol: String,
    pub event_type: String,
    pub node_id: String,
    pub timestamp: String,
    pub superstep: u64,
    pub payload_kind: String,
    pub payload_json: serde_json::Value,
    pub subgraph_namespace: Vec<String>,
}

/// Terminal state plus the exact LangGraph stream parts observed for a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionGraphStreamRun<S> {
    pub state: S,
    pub stream: Vec<DecisionGraphStreamPart>,
}

/// Emit Sentinel's typed custom-stream event for one decision graph node.
///
/// Decision graphs subscribe to LangGraph `Custom` stream mode and treat this
/// event as mandatory execution evidence. Missing writer scope is therefore a
/// node failure, not a best-effort telemetry miss.
pub(crate) fn emit_decision_node_event(
    graph_name: &str,
    node_id: &str,
    identifier: &str,
) -> Result<(), NodeError> {
    get_stream_writer()
        .ok_or_else(|| {
            NodeError::ExecutionFailed(format!(
                "LangGraph custom stream writer missing for {graph_name} decision node {node_id}"
            ))
        })?
        .write(serde_json::json!({
            "type": "sentinel.decision_node",
            "graph": graph_name,
            "node_id": node_id,
            "identifier": identifier,
        }))
        .map_err(|err| NodeError::ExecutionFailed(err.to_string()))
}

/// Serializable view of one durable LangGraph checkpoint snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionGraphCheckpointSnapshot<S> {
    pub checkpoint_id: String,
    pub parent_checkpoint_id: Option<String>,
    pub thread_id: String,
    pub created_at: String,
    pub step_number: u64,
    pub source_step: Option<i32>,
    pub source_type: Option<String>,
    pub source_node: Option<String>,
    pub tags: BTreeMap<String, String>,
    pub writes: Vec<DecisionGraphCheckpointWriteInfo>,
    pub state: S,
}

impl<S> DecisionGraphCheckpointSnapshot<S> {
    /// Stable audit reference for this concrete LangGraph checkpoint snapshot.
    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

/// Serializable view of one compiled decision-graph node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionGraphNodeInfo {
    pub id: String,
    pub deferred: bool,
    pub barrier_on: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub has_error_handler: bool,
    pub has_timeout_policy: bool,
    pub interrupt_before: bool,
    pub interrupt_after: bool,
}

/// Serializable view of one compiled decision-graph edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionGraphEdgeInfo {
    pub from: String,
    pub kind: String,
    pub to: Option<String>,
}

/// JSON schema contracts preserved by a compiled LangGraph decision graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionGraphSchemas {
    pub state: Option<serde_json::Value>,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub context: Option<serde_json::Value>,
}

/// Runtime topology for one compiled, checkpointed infrastructure decision graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionGraphTopology {
    pub graph: String,
    pub durable_checkpointer: bool,
    pub checkpointer_backend: String,
    pub checkpointer_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpointer_tenant_scope: Option<String>,
    pub auto_checkpoint: bool,
    pub max_iterations: usize,
    pub schemas: DecisionGraphSchemas,
    pub nodes: Vec<DecisionGraphNodeInfo>,
    pub edges: Vec<DecisionGraphEdgeInfo>,
}

/// Reflect a compiled LangGraph decision topology for logs, APIs, and tests.
pub fn topology<S>(
    graph_name: &str,
    compiled: &CompilationResult<S>,
) -> Result<DecisionGraphTopology, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let graph = &compiled.graph;
    let nodes: Vec<_> = graph
        .node_ids()
        .filter_map(|node_id| {
            let node = graph.node_introspection(node_id.as_str())?;
            let interrupt = graph.interrupt_config(node.id);
            Some(DecisionGraphNodeInfo {
                id: node.id.to_string(),
                deferred: node.deferred,
                barrier_on: node.barrier_on.into_iter().map(ToOwned::to_owned).collect(),
                metadata: node
                    .metadata
                    .into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string()))
                    .collect(),
                has_error_handler: node.has_error_handler,
                has_timeout_policy: node.has_timeout_policy,
                interrupt_before: interrupt.is_some_and(|cfg| cfg.interrupt_before()),
                interrupt_after: interrupt.is_some_and(|cfg| cfg.interrupt_after()),
            })
        })
        .collect();
    let checkpointer_backend = required_checkpointer_backend(graph_name, &nodes)?;
    let checkpointer_scope = required_checkpointer_scope(graph_name, &nodes)?;
    let checkpointer_tenant_scope =
        required_checkpointer_tenant_scope(graph_name, &nodes, &checkpointer_backend)?;
    required_decision_node_runtime_contract(graph_name, &nodes)?;

    let edges: Vec<DecisionGraphEdgeInfo> = graph
        .edge_descriptors()
        .map(|edge| DecisionGraphEdgeInfo {
            from: edge.from.to_string(),
            kind: format!("{:?}", edge.kind).to_ascii_lowercase(),
            to: edge.to.map(ToOwned::to_owned),
        })
        .collect();
    required_decision_edge_contract(graph_name, &nodes, &edges)?;
    let schemas = graph.schemas_json();
    required_decision_schema_contract(graph_name, &schemas.state, &schemas.input, &schemas.output)?;
    let durable_checkpointer = compiled.checkpointer.is_some();
    let auto_checkpoint = graph.auto_checkpoint();
    let max_iterations = graph.max_iterations();
    required_decision_runtime_contract(
        graph_name,
        durable_checkpointer,
        auto_checkpoint,
        max_iterations,
        nodes.len(),
    )?;

    Ok(DecisionGraphTopology {
        graph: graph_name.to_string(),
        durable_checkpointer,
        checkpointer_backend,
        checkpointer_scope,
        checkpointer_tenant_scope,
        auto_checkpoint,
        max_iterations,
        schemas: DecisionGraphSchemas {
            state: schemas.state,
            input: schemas.input,
            output: schemas.output,
            context: schemas.context,
        },
        nodes,
        edges,
    })
}

fn required_checkpointer_tenant_scope(
    graph_name: &str,
    nodes: &[DecisionGraphNodeInfo],
    backend: &str,
) -> Result<Option<String>, String> {
    let tenant_scope = required_uniform_metadata(
        graph_name,
        nodes,
        "sentinel.checkpointer_tenant_scope",
        "checkpointer tenant scope",
    )?;
    match backend {
        "sqlite" => {
            if tenant_scope.is_empty() {
                Ok(None)
            } else {
                Err(format!(
                    "{graph_name} SQLite decision graph must not carry hosted tenant metadata"
                ))
            }
        }
        "postgres" | "redis" => {
            if tenant_scope.is_empty() {
                return Err(format!(
                    "{graph_name} {backend} decision graph requires non-empty sentinel.checkpointer_tenant_scope metadata"
                ));
            }
            validate_tenant_scope(&tenant_scope)?;
            Ok(Some(tenant_scope))
        }
        other => Err(format!(
            "unsupported decision graph checkpointer backend '{other}' for {graph_name} topology"
        )),
    }
}

fn required_checkpointer_scope(
    graph_name: &str,
    nodes: &[DecisionGraphNodeInfo],
) -> Result<String, String> {
    required_uniform_metadata(
        graph_name,
        nodes,
        "sentinel.checkpointer_scope",
        "checkpointer scope",
    )
}

fn required_checkpointer_backend(
    graph_name: &str,
    nodes: &[DecisionGraphNodeInfo],
) -> Result<String, String> {
    required_uniform_metadata(
        graph_name,
        nodes,
        "sentinel.checkpointer_backend",
        "checkpointer backend",
    )
}

fn required_uniform_metadata(
    graph_name: &str,
    nodes: &[DecisionGraphNodeInfo],
    metadata_key: &str,
    label: &str,
) -> Result<String, String> {
    let mut backend: Option<&str> = None;
    for node in nodes {
        let Some(node_backend) = node.metadata.get(metadata_key) else {
            return Err(format!(
                "{graph_name} decision graph node '{}' is missing {metadata_key} metadata",
                node.id,
            ));
        };
        match backend {
            Some(expected) if expected != node_backend => {
                return Err(format!(
                    "{graph_name} decision graph node '{}' has {label} '{node_backend}', expected '{expected}'",
                    node.id,
                ));
            }
            Some(_) => {}
            None => backend = Some(node_backend),
        }
    }
    backend
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{graph_name} decision graph has no nodes"))
}

fn require_node_metadata_value(
    graph_name: &str,
    node: &DecisionGraphNodeInfo,
    metadata_key: &str,
    label: &str,
    expected: &str,
) -> Result<(), String> {
    let Some(value) = node.metadata.get(metadata_key) else {
        return Err(format!(
            "{graph_name} decision graph node '{}' is missing {metadata_key} metadata",
            node.id
        ));
    };
    if value != expected {
        return Err(format!(
            "{graph_name} decision graph node '{}' has {label} '{value}', expected '{expected}'",
            node.id
        ));
    }
    Ok(())
}

fn required_decision_node_runtime_contract(
    graph_name: &str,
    nodes: &[DecisionGraphNodeInfo],
) -> Result<(), String> {
    if nodes.is_empty() {
        return Err(format!("{graph_name} decision graph has no nodes"));
    }
    for node in nodes {
        require_node_metadata_value(graph_name, node, "sentinel.graph", "graph", graph_name)?;
        require_node_metadata_value(graph_name, node, "sentinel.node", "node", &node.id)?;
        if !node.has_timeout_policy {
            return Err(format!(
                "{graph_name} decision graph node '{}' is missing a LangGraph timeout policy",
                node.id
            ));
        }
    }
    Ok(())
}

fn required_decision_runtime_contract(
    graph_name: &str,
    durable_checkpointer: bool,
    auto_checkpoint: bool,
    max_iterations: usize,
    node_count: usize,
) -> Result<(), String> {
    if !durable_checkpointer {
        return Err(format!(
            "{graph_name} decision graph is missing a durable LangGraph checkpointer"
        ));
    }
    if !auto_checkpoint {
        return Err(format!(
            "{graph_name} decision graph disabled LangGraph auto-checkpointing"
        ));
    }
    if max_iterations <= node_count {
        return Err(format!(
            "{graph_name} decision graph has max_iterations {max_iterations}, expected more than {node_count} nodes"
        ));
    }
    Ok(())
}

fn required_schema_x_sentinel_value(
    graph_name: &str,
    state_schema: &serde_json::Value,
    key: &str,
    expected: &str,
) -> Result<(), String> {
    let Some(value) = state_schema
        .pointer(&format!("/x-sentinel/{key}"))
        .and_then(serde_json::Value::as_str)
    else {
        return Err(format!(
            "{graph_name} decision graph state schema is missing x-sentinel.{key}"
        ));
    };
    if value != expected {
        return Err(format!(
            "{graph_name} decision graph state schema has x-sentinel.{key} '{value}', expected '{expected}'"
        ));
    }
    Ok(())
}

fn required_decision_schema_contract(
    graph_name: &str,
    state: &Option<serde_json::Value>,
    input: &Option<serde_json::Value>,
    output: &Option<serde_json::Value>,
) -> Result<(), String> {
    let state_schema = state
        .as_ref()
        .ok_or_else(|| format!("{graph_name} decision graph is missing a state schema"))?;
    if input.is_none() {
        return Err(format!(
            "{graph_name} decision graph is missing an input schema"
        ));
    }
    if output.is_none() {
        return Err(format!(
            "{graph_name} decision graph is missing an output schema"
        ));
    }
    required_schema_x_sentinel_value(graph_name, state_schema, "graph", graph_name)?;
    required_schema_x_sentinel_value(graph_name, state_schema, "authority", "langgraph")?;
    Ok(())
}

fn required_decision_edge_contract(
    graph_name: &str,
    nodes: &[DecisionGraphNodeInfo],
    edges: &[DecisionGraphEdgeInfo],
) -> Result<(), String> {
    if edges.is_empty() {
        return Err(format!(
            "{graph_name} decision graph has no LangGraph edges"
        ));
    }

    let node_ids: BTreeSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    let mut known_sources = node_ids.clone();
    known_sources.insert(START);
    let mut known_targets = node_ids;
    known_targets.insert(END);

    let start_edges: Vec<_> = edges.iter().filter(|edge| edge.from == START).collect();
    if start_edges.is_empty() {
        return Err(format!(
            "{graph_name} decision graph is missing START routing"
        ));
    }
    if start_edges.len() > 1 {
        return Err(format!(
            "{graph_name} decision graph has duplicate START routing"
        ));
    }
    match start_edges[0].to.as_deref() {
        Some(target) if known_targets.contains(target) && target != END => {}
        Some(target) => {
            return Err(format!(
                "{graph_name} decision graph START edge targets unexpected node '{target}'"
            ));
        }
        None => {
            return Err(format!(
                "{graph_name} decision graph START edge must target a concrete node"
            ));
        }
    }

    if !edges.iter().any(|edge| edge.kind == "conditional") {
        return Err(format!(
            "{graph_name} decision graph has no conditional decision edge"
        ));
    }
    if !edges.iter().any(|edge| edge.to.as_deref() == Some(END)) {
        return Err(format!(
            "{graph_name} decision graph has no terminal edge to END"
        ));
    }

    for edge in edges {
        if edge.kind.trim().is_empty() {
            return Err(format!(
                "{graph_name} decision graph edge from '{}' has empty kind",
                edge.from
            ));
        }
        if !known_sources.contains(edge.from.as_str()) {
            return Err(format!(
                "{graph_name} decision graph has unexpected LangGraph edge source '{}'",
                edge.from
            ));
        }
        if edge.kind != "conditional" && edge.to.is_none() {
            return Err(format!(
                "{graph_name} decision graph non-conditional edge from '{}' is missing a target",
                edge.from
            ));
        }
        if let Some(target) = edge.to.as_deref() {
            if !known_targets.contains(target) {
                return Err(format!(
                    "{graph_name} decision graph edge from '{}' targets unexpected node '{target}'",
                    edge.from
                ));
            }
        }
    }

    Ok(())
}

/// Execute a decision graph through LangGraph's typed streaming surface.
///
/// # Errors
/// Returns a hard error if the stream emits a node/task error, closes without
/// `ExecutionComplete`, emits no stream parts, or does not include a terminal
/// values payload.
pub async fn stream_decision_run<S>(
    compiled: &CompilationResult<S>,
    thread_id: &str,
    graph_name: &str,
    identifier: &str,
    state: S,
) -> Result<DecisionGraphStreamRun<S>, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let checkpointer = compiled.checkpointer.as_ref().ok_or_else(|| {
        format!("decision graph {graph_name} stream requires a configured LangGraph checkpointer")
    })?;
    let config = serde_json::json!({
        "configurable": {
            "thread_id": thread_id,
        }
    });
    let handle = compiled
        .graph
        .stream_events_v3_with_options(
            state,
            decision_v3_transformers(),
            RunV3Options::new()
                .with_checkpointer(Arc::clone(checkpointer))
                .with_thread_id(thread_id.to_string())
                .with_config(config),
        )
        .await
        .map_err(|err| err.to_string())?;
    let run = collect_v3_decision_stream(handle, thread_id).await?;

    if run.stream.is_empty() {
        return Err(format!(
            "decision graph stream emitted no parts for thread {thread_id}"
        ));
    }
    if !run
        .stream
        .iter()
        .all(|part| part.stream_protocol == DECISION_STREAM_PROTOCOL)
    {
        return Err(format!(
            "decision graph stream for thread {thread_id} emitted non-v3 LangGraph stream evidence"
        ));
    }
    if !run
        .stream
        .iter()
        .any(|part| part.event_type == "ExecutionComplete")
    {
        return Err(format!(
            "decision graph stream for thread {thread_id} closed without ExecutionComplete"
        ));
    }
    require_decision_stream_payload_kind(&run.stream, thread_id, "values", "values")?;
    require_decision_stream_payload_kind(&run.stream, thread_id, "updates", "v3 updates")?;
    require_decision_stream_payload_kind(&run.stream, thread_id, "tasks", "v3 task")?;
    require_decision_stream_payload_kind(&run.stream, thread_id, "checkpoints", "checkpoint")?;
    require_decision_stream_payload_kind(&run.stream, thread_id, "debug", "v3 debug")?;
    if !run.stream.iter().any(|part| {
        part.payload_kind == "custom" && part.payload_json["type"] == "sentinel.decision_node"
    }) {
        return Err(format!(
            "decision graph stream for thread {thread_id} omitted Sentinel custom decision-node payloads"
        ));
    }
    validate_decision_custom_stream_events(&run.stream, thread_id, graph_name, identifier)?;

    Ok(run)
}

fn require_decision_stream_payload_kind(
    stream: &[DecisionGraphStreamPart],
    thread_id: &str,
    payload_kind: &str,
    label: &str,
) -> Result<(), String> {
    if stream.iter().any(|part| part.payload_kind == payload_kind) {
        Ok(())
    } else {
        Err(format!(
            "decision graph stream for thread {thread_id} omitted LangGraph {label} payloads"
        ))
    }
}

fn validate_decision_custom_stream_events(
    stream: &[DecisionGraphStreamPart],
    thread_id: &str,
    graph_name: &str,
    identifier: &str,
) -> Result<(), String> {
    let mut matching = 0usize;
    for part in stream
        .iter()
        .filter(|part| part.payload_kind == "custom")
        .filter(|part| part.payload_json["type"] == "sentinel.decision_node")
    {
        let payload_graph = part
            .payload_json
            .get("graph")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "decision graph stream for thread {thread_id} emitted Sentinel custom event without graph"
                )
            })?;
        if payload_graph != graph_name {
            return Err(format!(
                "decision graph stream for thread {thread_id} emitted custom event for graph '{payload_graph}', expected '{graph_name}'"
            ));
        }
        let payload_identifier = part
            .payload_json
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "decision graph stream for thread {thread_id} emitted Sentinel custom event without identifier"
                )
            })?;
        if payload_identifier != identifier {
            return Err(format!(
                "decision graph stream for thread {thread_id} emitted custom event for identifier '{payload_identifier}', expected '{identifier}'"
            ));
        }
        let payload_node = part
            .payload_json
            .get("node_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                format!(
                    "decision graph stream for thread {thread_id} emitted Sentinel custom event without node_id"
                )
            })?;
        if payload_node != part.node_id {
            return Err(format!(
                "decision graph stream for thread {thread_id} emitted custom event node '{payload_node}' on stream node '{}'",
                part.node_id
            ));
        }
        matching += 1;
    }

    if matching == 0 {
        return Err(format!(
            "decision graph stream for thread {thread_id} omitted Sentinel custom decision-node payloads for graph '{graph_name}' identifier '{identifier}'"
        ));
    }
    Ok(())
}

fn decision_v3_transformers() -> Vec<Arc<dyn V3StreamTransformer>> {
    vec![
        Arc::new(UpdatesTransformer),
        Arc::new(CheckpointsTransformer),
        Arc::new(TasksTransformer),
        Arc::new(DebugTransformer),
        Arc::new(CustomTransformer),
    ]
}

async fn collect_v3_decision_stream<S>(
    handle: GraphRunStream<S>,
    thread_id: &str,
) -> Result<DecisionGraphStreamRun<S>, String>
where
    S: Clone + Serialize + Send + 'static,
{
    let values_rx = handle.values().take().map_err(|err| err.to_string())?;
    let updates_rx = handle.updates().take().map_err(|err| err.to_string())?;
    let checkpoints_rx = handle.checkpoints().take().map_err(|err| err.to_string())?;
    let tasks_rx = handle.tasks().take().map_err(|err| err.to_string())?;
    let debug_rx = handle.debug().take().map_err(|err| err.to_string())?;
    let custom_rx = handle.custom().take().map_err(|err| err.to_string())?;
    let lifecycle_rx = handle.lifecycle().take().map_err(|err| err.to_string())?;
    let subgraphs_rx = handle.subgraphs().take().map_err(|err| err.to_string())?;
    let messages_rx = handle.messages().take().map_err(|err| err.to_string())?;

    let output = handle.output();
    let (
        values,
        updates,
        checkpoints,
        tasks,
        debug,
        custom,
        lifecycle,
        subgraphs,
        messages,
        output,
    ) = tokio::join!(
        drain_v3_values(values_rx),
        drain_v3_updates(updates_rx),
        drain_v3_checkpoints(checkpoints_rx),
        drain_v3_tasks(tasks_rx),
        drain_v3_debug(debug_rx),
        drain_v3_custom(custom_rx),
        drain_v3_lifecycle(lifecycle_rx),
        drain_v3_subgraphs(subgraphs_rx),
        drain_v3_messages(messages_rx),
        output,
    );

    let mut drain = V3ProjectionDrain::default();
    drain.extend(values?);
    drain.extend(updates?);
    drain.extend(checkpoints?);
    drain.extend(tasks?);
    drain.extend(debug?);
    drain.extend(custom?);
    drain.extend(lifecycle?);
    drain.extend(subgraphs?);
    drain.extend(messages?);

    if let Some(error) = drain.node_error {
        return Err(error);
    }

    let state = output.map_err(|err| {
        format!(
            "decision graph stream for thread {thread_id} closed without ExecutionComplete: {err}"
        )
    })?;
    let superstep = drain
        .stream
        .iter()
        .map(|part| part.superstep)
        .max()
        .unwrap_or(0)
        + 1;
    drain.stream.push(decision_stream_part(
        "ExecutionComplete",
        END,
        Utc::now().to_rfc3339(),
        superstep,
        "values",
        stream_payload_json("values", &state)?,
        Vec::new(),
    ));
    drain.stream.sort_by(|left, right| {
        left.superstep
            .cmp(&right.superstep)
            .then_with(|| left.timestamp.cmp(&right.timestamp))
            .then_with(|| stream_part_rank(left).cmp(&stream_part_rank(right)))
            .then_with(|| left.node_id.cmp(&right.node_id))
            .then_with(|| left.payload_kind.cmp(&right.payload_kind))
    });

    Ok(DecisionGraphStreamRun {
        state,
        stream: drain.stream,
    })
}

#[derive(Default)]
struct V3ProjectionDrain {
    stream: Vec<DecisionGraphStreamPart>,
    node_error: Option<String>,
}

impl V3ProjectionDrain {
    fn push(&mut self, part: DecisionGraphStreamPart) {
        if part.payload_kind == "tasks"
            && part
                .payload_json
                .get("status")
                .and_then(serde_json::Value::as_str)
                == Some("error")
        {
            self.node_error = Some(task_error_message(&part));
        }
        if part.event_type == "SubgraphFailed" {
            self.node_error = Some(lifecycle_error_message(&part));
        }
        self.stream.push(part);
    }

    fn extend(&mut self, other: V3ProjectionDrain) {
        if self.node_error.is_none() {
            self.node_error = other.node_error;
        }
        self.stream.extend(other.stream);
    }
}

fn task_error_message(part: &DecisionGraphStreamPart) -> String {
    part.payload_json
        .get("error")
        .and_then(serde_json::Value::as_str)
        .filter(|error| !error.trim().is_empty())
        .map(|error| {
            format!(
                "decision graph stream failed at node {}: {error}",
                part.node_id
            )
        })
        .unwrap_or_else(|| format!("decision graph stream failed at node {}", part.node_id))
}

fn lifecycle_error_message(part: &DecisionGraphStreamPart) -> String {
    part.payload_json
        .get("error")
        .and_then(serde_json::Value::as_str)
        .filter(|error| !error.trim().is_empty())
        .map(|error| {
            format!(
                "decision graph stream lifecycle failed at node {}: {error}",
                part.node_id
            )
        })
        .unwrap_or_else(|| {
            format!(
                "decision graph stream lifecycle failed at node {}",
                part.node_id
            )
        })
}

async fn drain_v3_values<S>(
    mut rx: mpsc::Receiver<StateSnapshot<S>>,
) -> Result<V3ProjectionDrain, String>
where
    S: Serialize,
{
    let mut drain = V3ProjectionDrain::default();
    while let Some(snapshot) = rx.recv().await {
        drain.push(decision_stream_part(
            "Values",
            STATE_SNAPSHOT_NODE,
            Utc::now().to_rfc3339(),
            snapshot.superstep(),
            "values",
            stream_payload_json("values", snapshot.state())?,
            Vec::new(),
        ));
    }
    Ok(drain)
}

async fn drain_v3_updates(
    mut rx: mpsc::Receiver<StateUpdateEvent>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(event) = rx.recv().await {
        drain.push(decision_stream_part(
            "Updates",
            event.node_id.to_string(),
            event.timestamp.to_rfc3339(),
            event.superstep,
            "updates",
            event.payload,
            namespace_strings(&event.subgraph_namespace),
        ));
    }
    Ok(drain)
}

async fn drain_v3_checkpoints(
    mut rx: mpsc::Receiver<CheckpointEvent>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(event) = rx.recv().await {
        drain.push(decision_stream_part(
            "Checkpoint",
            event.node_id.to_string(),
            event.timestamp.to_rfc3339(),
            event.superstep,
            "checkpoints",
            event.payload,
            namespace_strings(&event.subgraph_namespace),
        ));
    }
    Ok(drain)
}

async fn drain_v3_tasks(mut rx: mpsc::Receiver<TaskEvent>) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(event) = rx.recv().await {
        drain.push(decision_stream_part(
            "Task",
            event.node_id.to_string(),
            event.timestamp.to_rfc3339(),
            event.superstep,
            "tasks",
            event.payload,
            namespace_strings(&event.subgraph_namespace),
        ));
    }
    Ok(drain)
}

async fn drain_v3_debug(mut rx: mpsc::Receiver<DebugEvent>) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(event) = rx.recv().await {
        drain.push(decision_stream_part(
            "Debug",
            event.node_id.to_string(),
            event.timestamp.to_rfc3339(),
            event.superstep,
            "debug",
            stream_payload_json("debug", &event.info)?,
            namespace_strings(&event.subgraph_namespace),
        ));
    }
    Ok(drain)
}

async fn drain_v3_custom(mut rx: mpsc::Receiver<CustomEvent>) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(event) = rx.recv().await {
        drain.push(decision_stream_part(
            "Custom",
            event.node_id.to_string(),
            event.timestamp.to_rfc3339(),
            event.superstep,
            "custom",
            event.payload,
            namespace_strings(&event.subgraph_namespace),
        ));
    }
    Ok(drain)
}

async fn drain_v3_lifecycle(
    mut rx: mpsc::Receiver<LifecycleEvent>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(event) = rx.recv().await {
        let (event_type, node_id) = match &event {
            LifecycleEvent::SubgraphStarted { node_id, .. } => ("SubgraphStarted", node_id),
            LifecycleEvent::SubgraphCompleted { node_id, .. } => ("SubgraphCompleted", node_id),
            LifecycleEvent::SubgraphFailed { node_id, .. } => ("SubgraphFailed", node_id),
            _ => {
                return Err(
                    "unsupported non-exhaustive LangGraph v3 lifecycle event in decision stream"
                        .to_string(),
                );
            }
        };
        drain.push(decision_stream_part(
            event_type,
            node_id.to_string(),
            Utc::now().to_rfc3339(),
            0,
            "lifecycle",
            stream_payload_json("lifecycle", &event)?,
            namespace_strings(event.subgraph_namespace()),
        ));
    }
    Ok(drain)
}

async fn drain_v3_subgraphs(
    mut rx: mpsc::Receiver<SubgraphHandle>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(handle) = rx.recv().await {
        let full_path = handle
            .full_path()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        drain.push(decision_stream_part(
            "Subgraph",
            handle.node_id().to_string(),
            Utc::now().to_rfc3339(),
            0,
            "subgraphs",
            serde_json::json!({
                "node_id": handle.node_id().to_string(),
                "full_path": full_path,
            }),
            namespace_strings(handle.subgraph_namespace()),
        ));
    }
    Ok(drain)
}

async fn drain_v3_messages(
    mut rx: mpsc::Receiver<ChatModelStream>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(message) = rx.recv().await {
        let node_id = message.node_id.to_string();
        drain.push(decision_stream_part(
            "MessageStreamStarted",
            node_id.clone(),
            Utc::now().to_rfc3339(),
            0,
            "messages",
            serde_json::json!({
                "node_id": node_id.clone(),
                "stream": "chat_model",
            }),
            Vec::new(),
        ));
        let text_rx = Projection::take(&message.text).map_err(|err| err.to_string())?;
        let reasoning_rx = Projection::take(&message.reasoning).map_err(|err| err.to_string())?;
        let tool_calls_rx = Projection::take(&message.tool_calls).map_err(|err| err.to_string())?;
        let usage_rx = Projection::take(&message.usage).map_err(|err| err.to_string())?;
        let output = message.output();
        let (text, reasoning, tool_calls, usage, output) = tokio::join!(
            drain_chat_message_text(node_id.clone(), text_rx),
            drain_chat_message_reasoning(node_id.clone(), reasoning_rx),
            drain_chat_message_tool_calls(node_id.clone(), tool_calls_rx),
            drain_chat_message_usage(node_id.clone(), usage_rx),
            output,
        );
        drain.extend(text?);
        drain.extend(reasoning?);
        drain.extend(tool_calls?);
        drain.extend(usage?);

        let output = output.map_err(|err| {
            format!("decision graph message stream for node {node_id} closed without output: {err}")
        })?;
        drain.push(decision_stream_part(
            "MessageOutput",
            node_id,
            Utc::now().to_rfc3339(),
            0,
            "messages",
            serde_json::json!({
                "channel": "output",
                "message": stream_payload_json("message_output", &output)?,
            }),
            Vec::new(),
        ));
    }
    Ok(drain)
}

async fn drain_chat_message_text(
    node_id: String,
    mut rx: mpsc::Receiver<String>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(delta) = rx.recv().await {
        drain.push(decision_stream_part(
            "MessageTextDelta",
            node_id.clone(),
            Utc::now().to_rfc3339(),
            0,
            "messages",
            serde_json::json!({
                "channel": "text",
                "delta": delta,
            }),
            Vec::new(),
        ));
    }
    Ok(drain)
}

async fn drain_chat_message_reasoning(
    node_id: String,
    mut rx: mpsc::Receiver<String>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(delta) = rx.recv().await {
        drain.push(decision_stream_part(
            "MessageReasoningDelta",
            node_id.clone(),
            Utc::now().to_rfc3339(),
            0,
            "messages",
            serde_json::json!({
                "channel": "reasoning",
                "delta": delta,
            }),
            Vec::new(),
        ));
    }
    Ok(drain)
}

async fn drain_chat_message_tool_calls(
    node_id: String,
    mut rx: mpsc::Receiver<langgraph_core::domain::value_objects::ToolCall>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(tool_call) = rx.recv().await {
        drain.push(decision_stream_part(
            "MessageToolCall",
            node_id.clone(),
            Utc::now().to_rfc3339(),
            0,
            "messages",
            serde_json::json!({
                "channel": "tool_calls",
                "tool_call": stream_payload_json("message_tool_call", &tool_call)?,
            }),
            Vec::new(),
        ));
    }
    Ok(drain)
}

async fn drain_chat_message_usage(
    node_id: String,
    mut rx: mpsc::Receiver<langgraph_core::domain::value_objects::TokenUsage>,
) -> Result<V3ProjectionDrain, String> {
    let mut drain = V3ProjectionDrain::default();
    while let Some(usage) = rx.recv().await {
        drain.push(decision_stream_part(
            "MessageUsage",
            node_id.clone(),
            Utc::now().to_rfc3339(),
            0,
            "messages",
            serde_json::json!({
                "channel": "usage",
                "usage": stream_payload_json("message_usage", &usage)?,
            }),
            Vec::new(),
        ));
    }
    Ok(drain)
}

fn decision_stream_part(
    event_type: impl Into<String>,
    node_id: impl Into<String>,
    timestamp: String,
    superstep: u64,
    payload_kind: impl Into<String>,
    payload_json: serde_json::Value,
    subgraph_namespace: Vec<String>,
) -> DecisionGraphStreamPart {
    DecisionGraphStreamPart {
        stream_protocol: DECISION_STREAM_PROTOCOL.to_string(),
        event_type: event_type.into(),
        node_id: node_id.into(),
        timestamp,
        superstep,
        payload_kind: payload_kind.into(),
        payload_json,
        subgraph_namespace,
    }
}

fn namespace_strings(namespace: &[langgraph_core::domain::value_objects::NodeId]) -> Vec<String> {
    namespace.iter().map(ToString::to_string).collect()
}

fn stream_part_rank(part: &DecisionGraphStreamPart) -> u8 {
    match part.event_type.as_str() {
        "Values" => 0,
        "Updates" => 1,
        "Task" => 2,
        "Debug" => 3,
        "Custom" => 4,
        "Checkpoint" => 5,
        "SubgraphStarted" => 6,
        "SubgraphCompleted" => 7,
        "SubgraphFailed" => 8,
        "Subgraph" => 9,
        "MessageStreamStarted" => 10,
        "MessageTextDelta" => 11,
        "MessageReasoningDelta" => 12,
        "MessageToolCall" => 13,
        "MessageUsage" => 14,
        "MessageOutput" => 15,
        "ExecutionComplete" => 16,
        _ => 17,
    }
}

fn stream_payload_json<T: Serialize + ?Sized>(
    payload_kind: &str,
    value: &T,
) -> Result<serde_json::Value, String> {
    serde_json::to_value(value)
        .map_err(|err| format!("failed to serialize LangGraph {payload_kind} payload: {err}"))
}

/// Return the persisted checkpoint trail for a decision graph run.
///
/// The returned order is normalized to latest checkpoint first.
///
/// # Errors
/// Returns the stringified LangGraph error, a thread mismatch, or an empty
/// history error if the graph did not persist checkpoints for the run.
pub async fn checkpoint_history<S>(
    compiled: &CompilationResult<S>,
    thread_id: &str,
) -> Result<Vec<DecisionGraphCheckpointSnapshot<S>>, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let snapshots = compiled
        .get_state_history(thread_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut history = Vec::with_capacity(snapshots.len());

    for snapshot in snapshots {
        let metadata = snapshot.metadata();
        if metadata.thread_id() != thread_id {
            return Err(format!(
                "LangGraph checkpoint thread mismatch: expected {thread_id}, got {}",
                metadata.thread_id()
            ));
        }

        let (source_step, source_type, source_node) = metadata
            .source()
            .map(|source| {
                (
                    Some(source.step()),
                    Some(source.source_type().to_string()),
                    source.node().map(ToOwned::to_owned),
                )
            })
            .unwrap_or((None, None, None));

        history.push(DecisionGraphCheckpointSnapshot {
            checkpoint_id: metadata.checkpoint_id().to_string(),
            parent_checkpoint_id: metadata
                .parent_checkpoint_id()
                .map(|checkpoint_id| checkpoint_id.to_string()),
            thread_id: metadata.thread_id().to_string(),
            created_at: metadata.created_at().to_rfc3339(),
            step_number: metadata.step_number(),
            source_step,
            source_type,
            source_node,
            tags: metadata
                .tags()
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            writes: metadata
                .writes()
                .iter()
                .map(|write| DecisionGraphCheckpointWriteInfo {
                    node_id: write.node_id().to_string(),
                    channel: write.channel().to_string(),
                    ts: write.ts().to_rfc3339(),
                })
                .collect(),
            state: snapshot.state().clone(),
        });
    }

    if history.is_empty() {
        return Err(format!(
            "LangGraph checkpoint history is empty for thread {thread_id}"
        ));
    }

    history.sort_by(|left, right| {
        right
            .step_number
            .cmp(&left.step_number)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| right.checkpoint_id.cmp(&left.checkpoint_id))
    });

    Ok(history)
}

/// Stream LangGraph checkpoint writes for a decision graph run.
///
/// This uses the upstream `CheckpointSaver::get_writes_history` audit channel
/// instead of reconstructing values from checkpoint metadata. The result
/// includes content length, SHA-256, and decoded JSON values when the serialized
/// checkpoint channel is JSON.
///
/// # Errors
/// Returns a hard error if the graph was not compiled with a checkpointer, if
/// the underlying checkpoint write stream fails, if no writes are present for
/// the requested channel, or if a recorded write lacks its serialized value.
pub async fn write_history<S>(
    compiled: &CompilationResult<S>,
    thread_id: &str,
    channel: Option<&str>,
) -> Result<Vec<DecisionGraphWriteHistoryEntry>, String>
where
    S: Clone + Serialize + DeserializeOwned + Send + Sync + 'static,
{
    let checkpointer = compiled.checkpointer.as_ref().ok_or_else(|| {
        "decision graph write history requires a configured checkpointer".to_string()
    })?;
    let mut stream = checkpointer.get_writes_history(thread_id, channel);
    let mut entries = Vec::new();

    while let Some(entry) = stream.next().await {
        let entry = entry.map_err(|err| err.to_string())?;
        entries.push(write_history_view(thread_id, entry)?);
    }

    if entries.is_empty() {
        let channel_label = match channel {
            Some(channel) => format!(" channel {channel}"),
            None => String::new(),
        };
        return Err(format!(
            "LangGraph checkpoint write history is empty for thread {thread_id}{channel_label}"
        ));
    }

    if channel.is_none() && !entries.iter().any(|entry| entry.channel == "state") {
        return Err(format!(
            "LangGraph checkpoint write history for thread {thread_id} does not include state channel writes"
        ));
    }

    Ok(entries)
}

/// Validate that a decision graph's terminal state is durably backed by LangGraph.
///
/// The report-level state is only accepted when the latest checkpoint carries
/// the same state, the checkpoint write-history stream contains the matching
/// state-channel write for that exact checkpoint, and the latest real decision
/// node that wrote that terminal state emitted matching custom stream evidence.
///
/// # Errors
/// Returns a hard error if checkpoint history, checkpoint metadata, or
/// write-history evidence is missing or does not match the accepted terminal
/// state.
pub fn validate_decision_graph_run<S>(
    graph_name: &str,
    thread_id: &str,
    terminal_state: &S,
    stream: &[DecisionGraphStreamPart],
    checkpoints: &[DecisionGraphCheckpointSnapshot<S>],
    write_history: &[DecisionGraphWriteHistoryEntry],
) -> Result<(), String>
where
    S: Serialize,
{
    let latest = checkpoints.first().ok_or_else(|| {
        format!("{graph_name} decision graph omitted checkpoint history for thread '{thread_id}'")
    })?;
    if latest.thread_id != thread_id {
        return Err(format!(
            "{graph_name} decision graph latest checkpoint thread mismatch: expected '{thread_id}', got '{}'",
            latest.thread_id
        ));
    }
    if let Some(mismatched) = checkpoints
        .iter()
        .find(|checkpoint| checkpoint.thread_id != thread_id)
    {
        return Err(format!(
            "{graph_name} decision graph checkpoint history contains thread '{}' while validating thread '{thread_id}'",
            mismatched.thread_id
        ));
    }
    for pair in checkpoints.windows(2) {
        if pair[0].step_number < pair[1].step_number {
            return Err(format!(
                "{graph_name} decision graph checkpoint history for thread '{thread_id}' is not latest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                pair[0].checkpoint_id,
                pair[0].step_number,
                pair[1].checkpoint_id,
                pair[1].step_number
            ));
        }
    }
    if let Some(mismatched) = write_history
        .iter()
        .find(|write| write.thread_id != thread_id)
    {
        return Err(format!(
            "{graph_name} decision graph write history contains thread '{}' while validating thread '{thread_id}'",
            mismatched.thread_id
        ));
    }
    for pair in write_history.windows(2) {
        if pair[0].step_number > pair[1].step_number {
            return Err(format!(
                "{graph_name} decision graph write history for thread '{thread_id}' is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                pair[0].checkpoint_id,
                pair[0].step_number,
                pair[1].checkpoint_id,
                pair[1].step_number
            ));
        }
    }
    if let Some(part) = stream
        .iter()
        .find(|part| part.stream_protocol != DECISION_STREAM_PROTOCOL)
    {
        return Err(format!(
            "{graph_name} decision graph stream for thread '{thread_id}' contains {} evidence, expected {DECISION_STREAM_PROTOCOL}",
            part.stream_protocol
        ));
    }

    let terminal_json = serde_json::to_value(terminal_state).map_err(|err| {
        format!(
            "{graph_name} decision graph failed to serialize terminal state for thread '{thread_id}': {err}"
        )
    })?;
    let checkpoint_json = serde_json::to_value(&latest.state).map_err(|err| {
        format!(
            "{graph_name} decision graph failed to serialize latest checkpoint state for thread '{thread_id}': {err}"
        )
    })?;
    if checkpoint_json != terminal_json {
        return Err(format!(
            "{graph_name} decision graph latest checkpoint state mismatch for thread '{thread_id}'"
        ));
    }
    validate_latest_checkpoint_stream_evidence(
        graph_name,
        thread_id,
        latest,
        stream,
        &terminal_json,
    )?;

    let latest_state_writes: Vec<_> = latest
        .writes
        .iter()
        .filter(|write| write.channel == "state")
        .collect();
    if latest_state_writes.is_empty() {
        return Err(format!(
            "{graph_name} decision graph latest checkpoint '{}' omitted state-channel write metadata for thread '{thread_id}'",
            latest.checkpoint_id
        ));
    }

    let terminal_write = write_history
        .iter()
        .find(|write| {
            write.checkpoint_id == latest.checkpoint_id
                && write.channel == "state"
                && latest_state_writes
                    .iter()
                    .any(|metadata| metadata.node_id == write.node_id && metadata.ts == write.ts)
        })
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph write history omitted latest state-channel write for checkpoint '{}' thread '{thread_id}'",
                latest.checkpoint_id
            )
        })?;
    if &terminal_write.value_json != &terminal_json {
        return Err(format!(
            "{graph_name} decision graph latest state-channel write mismatch for checkpoint '{}' thread '{thread_id}'",
            latest.checkpoint_id
        ));
    }

    let terminal_identifier = terminal_json
        .get("identifier")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph terminal state omitted identifier for thread '{thread_id}'"
            )
        })?;
    let terminal_decision_write =
        latest_terminal_decision_write(graph_name, thread_id, write_history, &terminal_json)?;
    let _terminal_decision_checkpoint = terminal_decision_checkpoint_from_write(
        graph_name,
        thread_id,
        &terminal_json,
        checkpoints,
        terminal_decision_write,
    )?;
    validate_decision_terminal_custom_stream_event(
        stream,
        thread_id,
        graph_name,
        terminal_identifier,
        &terminal_decision_write.node_id,
    )?;

    Ok(())
}

pub(crate) fn terminal_decision_checkpoint_result<'a, S>(
    graph_name: &str,
    thread_id: &str,
    terminal_state: &S,
    checkpoints: &'a [DecisionGraphCheckpointSnapshot<S>],
    write_history: &[DecisionGraphWriteHistoryEntry],
) -> Result<&'a DecisionGraphCheckpointSnapshot<S>, String>
where
    S: Serialize,
{
    let terminal_json = serde_json::to_value(terminal_state).map_err(|err| {
        format!("{graph_name} decision graph failed to serialize terminal state for thread '{thread_id}': {err}")
    })?;
    let decision_write =
        latest_terminal_decision_write(graph_name, thread_id, write_history, &terminal_json)?;
    terminal_decision_checkpoint_from_write(
        graph_name,
        thread_id,
        &terminal_json,
        checkpoints,
        decision_write,
    )
}

fn terminal_decision_checkpoint_from_write<'a, S>(
    graph_name: &str,
    thread_id: &str,
    terminal_json: &serde_json::Value,
    checkpoints: &'a [DecisionGraphCheckpointSnapshot<S>],
    decision_write: &DecisionGraphWriteHistoryEntry,
) -> Result<&'a DecisionGraphCheckpointSnapshot<S>, String>
where
    S: Serialize,
{
    let checkpoint = checkpoints
        .iter()
        .find(|checkpoint| {
            checkpoint.thread_id == thread_id
                && checkpoint.checkpoint_id == decision_write.checkpoint_id
                && checkpoint.writes.iter().any(|metadata| {
                    metadata.channel == "state"
                        && metadata.node_id == decision_write.node_id
                        && metadata.ts == decision_write.ts
                })
        })
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph checkpoint history omitted terminal decision-node checkpoint '{}' for thread '{thread_id}'",
                decision_write.checkpoint_id
            )
        })?;
    let checkpoint_json = serde_json::to_value(&checkpoint.state).map_err(|err| {
        format!(
            "{graph_name} decision graph failed to serialize terminal decision-node checkpoint '{}' state for thread '{thread_id}': {err}",
            checkpoint.checkpoint_id
        )
    })?;
    if &checkpoint_json != terminal_json {
        return Err(format!(
            "{graph_name} decision graph terminal decision-node checkpoint '{}' state mismatch for thread '{thread_id}'",
            checkpoint.checkpoint_id
        ));
    }
    Ok(checkpoint)
}

fn latest_terminal_decision_write<'a>(
    graph_name: &str,
    thread_id: &str,
    write_history: &'a [DecisionGraphWriteHistoryEntry],
    terminal_json: &serde_json::Value,
) -> Result<&'a DecisionGraphWriteHistoryEntry, String> {
    write_history
        .iter()
        .rev()
        .find(|write| {
            write.channel == "state"
                && &write.value_json == terminal_json
                && !is_graph_boundary_node(&write.node_id)
        })
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph write history omitted terminal decision-node state write for thread '{thread_id}'"
            )
        })
}

fn is_graph_boundary_node(node_id: &str) -> bool {
    node_id == START || node_id == END
}

fn validate_decision_terminal_custom_stream_event(
    stream: &[DecisionGraphStreamPart],
    thread_id: &str,
    graph_name: &str,
    identifier: &str,
    expected_node: &str,
) -> Result<(), String> {
    validate_decision_custom_stream_events(stream, thread_id, graph_name, identifier)?;
    if stream.iter().any(|part| {
        part.payload_kind == "custom"
            && part.node_id == expected_node
            && part
                .payload_json
                .get("type")
                .and_then(serde_json::Value::as_str)
                == Some("sentinel.decision_node")
            && part
                .payload_json
                .get("graph")
                .and_then(serde_json::Value::as_str)
                == Some(graph_name)
            && part
                .payload_json
                .get("identifier")
                .and_then(serde_json::Value::as_str)
                == Some(identifier)
            && part
                .payload_json
                .get("node_id")
                .and_then(serde_json::Value::as_str)
                == Some(expected_node)
    }) {
        return Ok(());
    }

    Err(format!(
        "{graph_name} decision graph stream for thread {thread_id} omitted terminal custom decision-node payload for node '{expected_node}' identifier '{identifier}'"
    ))
}

fn validate_latest_checkpoint_stream_evidence<S>(
    graph_name: &str,
    thread_id: &str,
    latest: &DecisionGraphCheckpointSnapshot<S>,
    stream: &[DecisionGraphStreamPart],
    terminal_json: &serde_json::Value,
) -> Result<(), String> {
    let checkpoint_part = stream
        .iter()
        .filter(|part| part.payload_kind == "checkpoints")
        .find(|part| {
            part.payload_json
                .get("checkpoint_id")
                .and_then(serde_json::Value::as_str)
                == Some(latest.checkpoint_id.as_str())
        })
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph stream omitted latest checkpoint payload for checkpoint '{}' thread '{thread_id}'",
                latest.checkpoint_id
            )
        })?;

    if checkpoint_part.event_type != "Checkpoint" {
        return Err(format!(
            "{graph_name} decision graph stream payload for checkpoint '{}' has event type '{}', expected 'Checkpoint'",
            latest.checkpoint_id, checkpoint_part.event_type
        ));
    }

    let stream_thread = required_payload_str(
        graph_name,
        thread_id,
        &latest.checkpoint_id,
        &checkpoint_part.payload_json,
        "thread_id",
    )?;
    if stream_thread != thread_id {
        return Err(format!(
            "{graph_name} decision graph stream checkpoint '{}' thread mismatch: expected '{thread_id}', got '{stream_thread}'",
            latest.checkpoint_id
        ));
    }

    let stream_step = checkpoint_part
        .payload_json
        .get("step_number")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph stream checkpoint '{}' omitted numeric step_number for thread '{thread_id}'",
                latest.checkpoint_id
            )
        })?;
    if stream_step != latest.step_number {
        return Err(format!(
            "{graph_name} decision graph stream checkpoint '{}' step mismatch: expected {}, got {}",
            latest.checkpoint_id, latest.step_number, stream_step
        ));
    }

    let stream_parent = optional_payload_str(
        graph_name,
        thread_id,
        &latest.checkpoint_id,
        &checkpoint_part.payload_json,
        "parent_checkpoint_id",
    )?;
    if stream_parent != latest.parent_checkpoint_id.as_deref() {
        return Err(format!(
            "{graph_name} decision graph stream checkpoint '{}' parent mismatch for thread '{thread_id}'",
            latest.checkpoint_id
        ));
    }

    let latest_source_type = latest.source_type.as_deref().ok_or_else(|| {
        format!(
            "{graph_name} decision graph latest checkpoint '{}' omitted source type for thread '{thread_id}'",
            latest.checkpoint_id
        )
    })?;
    let stream_source_type = checkpoint_part
        .payload_json
        .pointer("/source/type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph stream checkpoint '{}' omitted source type for thread '{thread_id}'",
                latest.checkpoint_id
            )
        })?;
    if stream_source_type != latest_source_type {
        return Err(format!(
            "{graph_name} decision graph stream checkpoint '{}' source type mismatch: expected '{latest_source_type}', got '{stream_source_type}'",
            latest.checkpoint_id
        ));
    }

    let latest_source_node = latest.source_node.as_deref().ok_or_else(|| {
        format!(
            "{graph_name} decision graph latest checkpoint '{}' omitted source node for thread '{thread_id}'",
            latest.checkpoint_id
        )
    })?;
    let stream_source_node = checkpoint_part
        .payload_json
        .pointer("/source/node")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph stream checkpoint '{}' omitted source node for thread '{thread_id}'",
                latest.checkpoint_id
            )
        })?;
    if stream_source_node != latest_source_node {
        return Err(format!(
            "{graph_name} decision graph stream checkpoint '{}' source node mismatch: expected '{latest_source_node}', got '{stream_source_node}'",
            latest.checkpoint_id
        ));
    }

    let stream_state = checkpoint_part.payload_json.get("state").ok_or_else(|| {
        format!(
            "{graph_name} decision graph stream checkpoint '{}' omitted state for thread '{thread_id}'",
            latest.checkpoint_id
        )
    })?;
    if stream_state != terminal_json {
        return Err(format!(
            "{graph_name} decision graph stream checkpoint '{}' state mismatch for thread '{thread_id}'",
            latest.checkpoint_id
        ));
    }

    Ok(())
}

fn required_payload_str<'a>(
    graph_name: &str,
    thread_id: &str,
    checkpoint_id: &str,
    payload: &'a serde_json::Value,
    field: &str,
) -> Result<&'a str, String> {
    payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "{graph_name} decision graph stream checkpoint '{checkpoint_id}' omitted string {field} for thread '{thread_id}'"
            )
        })
}

fn optional_payload_str<'a>(
    graph_name: &str,
    thread_id: &str,
    checkpoint_id: &str,
    payload: &'a serde_json::Value,
    field: &str,
) -> Result<Option<&'a str>, String> {
    match payload.get(field) {
        Some(serde_json::Value::String(value)) => Ok(Some(value.as_str())),
        Some(serde_json::Value::Null) => Ok(None),
        Some(_) => Err(format!(
            "{graph_name} decision graph stream checkpoint '{checkpoint_id}' emitted non-string {field} for thread '{thread_id}'"
        )),
        None => Err(format!(
            "{graph_name} decision graph stream checkpoint '{checkpoint_id}' omitted {field} for thread '{thread_id}'"
        )),
    }
}

fn write_history_view(
    thread_id: &str,
    entry: WriteHistoryEntry,
) -> Result<DecisionGraphWriteHistoryEntry, String> {
    let bytes = entry.value.as_ref().ok_or_else(|| {
        format!(
            "LangGraph checkpoint write {} step {} channel {} node {} is missing its serialized value",
            entry.checkpoint_id, entry.step_number, entry.channel, entry.node_id
        )
    })?;
    let value_json = serde_json::from_slice::<serde_json::Value>(bytes).map_err(|err| {
        format!(
            "LangGraph checkpoint write {} step {} channel {} node {} is not valid JSON: {err}",
            entry.checkpoint_id, entry.step_number, entry.channel, entry.node_id
        )
    })?;

    Ok(DecisionGraphWriteHistoryEntry {
        thread_id: thread_id.to_string(),
        checkpoint_id: entry.checkpoint_id.to_string(),
        step_number: entry.step_number,
        channel: entry.channel,
        node_id: entry.node_id,
        ts: entry.ts.to_rfc3339(),
        value_len: bytes.len(),
        value_sha256: sha256_hex(bytes),
        value_json,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to String must succeed");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, backend: Option<&str>, scope: Option<&str>) -> DecisionGraphNodeInfo {
        let mut metadata = BTreeMap::new();
        metadata.insert("sentinel.graph".to_string(), "severity".to_string());
        metadata.insert("sentinel.node".to_string(), id.to_string());
        if let Some(backend) = backend {
            metadata.insert(
                "sentinel.checkpointer_backend".to_string(),
                backend.to_string(),
            );
        }
        if let Some(scope) = scope {
            metadata.insert("sentinel.checkpointer_scope".to_string(), scope.to_string());
        }
        metadata.insert(
            "sentinel.checkpointer_tenant_scope".to_string(),
            String::new(),
        );
        DecisionGraphNodeInfo {
            id: id.to_string(),
            deferred: false,
            barrier_on: Vec::new(),
            metadata,
            has_error_handler: false,
            has_timeout_policy: true,
            interrupt_before: false,
            interrupt_after: false,
        }
    }

    #[test]
    fn validate_decision_graph_run_accepts_matching_terminal_evidence() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let older = checkpoint(
            "thread-1",
            "checkpoint-1",
            1,
            serde_json::json!({
                "identifier": "FPCRM-1",
                "decision": "Skip"
            }),
            "classify",
        );
        let writes = vec![
            write_entry(
                "checkpoint-1",
                1,
                "classify",
                serde_json::json!({
                    "identifier": "FPCRM-1",
                    "decision": "Skip"
                }),
            ),
            write_entry("checkpoint-2", 2, "set", terminal.clone()),
        ];
        let stream = vec![
            checkpoint_part(&latest),
            custom_part("set", "severity", "FPCRM-1"),
        ];

        validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest, older],
            &writes,
        )
        .expect(
            "matching terminal state, stream checkpoint, latest checkpoint, and write history must pass",
        );
    }

    #[test]
    fn validate_decision_graph_run_accepts_end_checkpoint_with_decision_node_write() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), END);
        let decision = checkpoint("thread-1", "checkpoint-1", 1, terminal.clone(), "set");
        let writes = vec![
            write_entry("checkpoint-1", 1, "set", terminal.clone()),
            write_entry("checkpoint-2", 2, END, terminal.clone()),
        ];
        let stream = vec![
            checkpoint_part(&latest),
            custom_part("set", "severity", "FPCRM-1"),
        ];

        validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest, decision],
            &writes,
        )
        .expect("END checkpoint with matching terminal decision-node custom event must pass");
    }

    #[test]
    fn terminal_decision_checkpoint_returns_decision_node_checkpoint() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), END);
        let decision = checkpoint("thread-1", "checkpoint-1", 1, terminal.clone(), "set");
        let writes = vec![
            write_entry("checkpoint-1", 1, "set", terminal.clone()),
            write_entry("checkpoint-2", 2, END, terminal.clone()),
        ];
        let checkpoints = vec![latest, decision];

        let checkpoint = terminal_decision_checkpoint_result(
            "severity",
            "thread-1",
            &terminal,
            &checkpoints,
            &writes,
        )
        .expect("terminal decision checkpoint must resolve");

        assert_eq!(checkpoint.checkpoint_id, "checkpoint-1");
        assert_eq!(checkpoint.source_node.as_deref(), Some("set"));
    }

    #[test]
    fn terminal_decision_checkpoint_rejects_boundary_only_write_history() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), END);
        let writes = vec![write_entry("checkpoint-2", 2, END, terminal.clone())];

        let err = terminal_decision_checkpoint_result(
            "severity",
            "thread-1",
            &terminal,
            &[latest],
            &writes,
        )
        .expect_err("boundary-only write history must fail strict checkpoint lookup");
        assert!(
            err.contains("write history omitted terminal decision-node state write"),
            "{err}"
        );
    }

    #[test]
    fn validate_decision_graph_run_rejects_stale_latest_checkpoint() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint(
            "thread-1",
            "checkpoint-2",
            2,
            serde_json::json!({
                "identifier": "FPCRM-1",
                "decision": "Skip"
            }),
            "skip",
        );
        let writes = vec![write_entry(
            "checkpoint-2",
            2,
            "skip",
            serde_json::json!({
                "identifier": "FPCRM-1",
                "decision": "Skip"
            }),
        )];
        let stream = vec![checkpoint_part(&latest)];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("stale latest checkpoint must fail");
        assert!(err.contains("latest checkpoint state mismatch"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_forged_terminal_write_history() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let writes = vec![write_entry(
            "checkpoint-2",
            2,
            "set",
            serde_json::json!({
                "identifier": "FPCRM-1",
                "decision": "Skip"
            }),
        )];
        let stream = vec![checkpoint_part(&latest)];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("forged write history must fail");
        assert!(err.contains("latest state-channel write mismatch"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_terminal_write_from_previous_checkpoint() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let writes = vec![write_entry("checkpoint-1", 1, "set", terminal.clone())];
        let stream = vec![checkpoint_part(&latest)];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("terminal write from an older checkpoint must fail");
        assert!(err.contains("omitted latest state-channel write"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_mismatched_write_thread() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let mut writes = vec![write_entry("checkpoint-2", 2, "set", terminal.clone())];
        writes[0].thread_id = "thread-2".to_string();
        let stream = vec![checkpoint_part(&latest)];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("mismatched write thread must fail");
        assert!(err.contains("write history contains thread 'thread-2'"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_non_v3_stream_evidence() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let writes = vec![write_entry("checkpoint-2", 2, "set", terminal.clone())];
        let mut stream = vec![
            checkpoint_part(&latest),
            custom_part("set", "severity", "FPCRM-1"),
        ];
        stream[0].stream_protocol = "v2".to_string();

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("non-v3 stream evidence must fail");
        assert!(err.contains("expected v3"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_out_of_order_write_history() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let stream = vec![checkpoint_part(&latest)];
        let writes = vec![
            write_entry("checkpoint-2", 2, "set", terminal.clone()),
            write_entry(
                "checkpoint-1",
                1,
                "classify",
                serde_json::json!({
                    "identifier": "FPCRM-1",
                    "decision": "Skip"
                }),
            ),
        ];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("out-of-order write history must fail");
        assert!(err.contains("write history for thread 'thread-1' is not oldest-first"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_out_of_order_checkpoint_history() {
        let older = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Skip"
        });
        let latest = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let older_checkpoint = checkpoint("thread-1", "checkpoint-1", 1, older.clone(), "skip");
        let latest_checkpoint = checkpoint("thread-1", "checkpoint-2", 2, latest, "set");
        let writes = vec![write_entry("checkpoint-1", 1, "skip", older.clone())];
        let stream = vec![checkpoint_part(&older_checkpoint)];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &older,
            &stream,
            &[older_checkpoint, latest_checkpoint],
            &writes,
        )
        .expect_err("out-of-order checkpoint history must fail");
        assert!(err.contains("not latest-first"));
    }

    #[test]
    fn validate_decision_graph_run_requires_latest_state_write_metadata() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let mut latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let stream = vec![checkpoint_part(&latest)];
        latest.writes.clear();
        let writes = vec![write_entry("checkpoint-2", 2, "set", terminal.clone())];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("missing checkpoint write metadata must fail");
        assert!(err.contains("omitted state-channel write metadata"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_missing_latest_stream_checkpoint() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let previous = checkpoint("thread-1", "checkpoint-1", 1, terminal.clone(), "set");
        let stream = vec![checkpoint_part(&previous)];
        let writes = vec![write_entry("checkpoint-2", 2, "set", terminal.clone())];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("missing latest checkpoint stream payload must fail");
        assert!(err.contains("stream omitted latest checkpoint payload"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_mismatched_stream_checkpoint_thread() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let mut stream_part = checkpoint_part(&latest);
        stream_part.payload_json["thread_id"] = serde_json::json!("thread-2");
        let stream = vec![stream_part];
        let writes = vec![write_entry("checkpoint-2", 2, "set", terminal.clone())];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("mismatched checkpoint stream thread must fail");
        assert!(err.contains("stream checkpoint 'checkpoint-2' thread mismatch"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_forged_stream_checkpoint_state() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), "set");
        let mut stream_part = checkpoint_part(&latest);
        stream_part.payload_json["state"] = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Skip"
        });
        let stream = vec![stream_part];
        let writes = vec![write_entry("checkpoint-2", 2, "set", terminal.clone())];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("forged checkpoint stream state must fail");
        assert!(err.contains("stream checkpoint 'checkpoint-2' state mismatch"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_missing_terminal_decision_checkpoint() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), END);
        let writes = vec![
            write_entry("checkpoint-1", 1, "set", terminal.clone()),
            write_entry("checkpoint-2", 2, END, terminal.clone()),
        ];
        let stream = vec![
            checkpoint_part(&latest),
            custom_part("set", "severity", "FPCRM-1"),
        ];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest],
            &writes,
        )
        .expect_err("missing terminal decision-node checkpoint must fail");
        assert!(err.contains("omitted terminal decision-node checkpoint"));
    }

    #[test]
    fn validate_decision_graph_run_rejects_missing_terminal_custom_event() {
        let terminal = serde_json::json!({
            "identifier": "FPCRM-1",
            "decision": "Set"
        });
        let latest = checkpoint("thread-1", "checkpoint-2", 2, terminal.clone(), END);
        let decision = checkpoint("thread-1", "checkpoint-1", 1, terminal.clone(), "set");
        let writes = vec![
            write_entry("checkpoint-1", 1, "set", terminal.clone()),
            write_entry("checkpoint-2", 2, END, terminal.clone()),
        ];
        let stream = vec![
            checkpoint_part(&latest),
            custom_part("classify", "severity", "FPCRM-1"),
        ];

        let err = validate_decision_graph_run(
            "severity",
            "thread-1",
            &terminal,
            &stream,
            &[latest, decision],
            &writes,
        )
        .expect_err("custom event from an older decision node must fail");
        assert!(err.contains("omitted terminal custom decision-node payload"));
    }

    #[test]
    fn validate_decision_custom_stream_events_accepts_matching_payload() {
        let stream = vec![custom_part("classify", "severity", "FPCRM-1")];

        validate_decision_custom_stream_events(&stream, "thread-1", "severity", "FPCRM-1")
            .expect("matching custom event must pass");
    }

    #[test]
    fn validate_decision_custom_stream_events_rejects_wrong_graph() {
        let stream = vec![custom_part("classify", "enforcement", "FPCRM-1")];

        let err =
            validate_decision_custom_stream_events(&stream, "thread-1", "severity", "FPCRM-1")
                .expect_err("wrong graph custom event must fail");
        assert!(err.contains("expected 'severity'"));
    }

    #[test]
    fn validate_decision_custom_stream_events_rejects_wrong_identifier() {
        let stream = vec![custom_part("classify", "severity", "FPCRM-2")];

        let err =
            validate_decision_custom_stream_events(&stream, "thread-1", "severity", "FPCRM-1")
                .expect_err("wrong identifier custom event must fail");
        assert!(err.contains("expected 'FPCRM-1'"));
    }

    #[test]
    fn validate_decision_custom_stream_events_rejects_mismatched_node() {
        let mut part = custom_part("classify", "severity", "FPCRM-1");
        part.payload_json["node_id"] = serde_json::json!("set");

        let err =
            validate_decision_custom_stream_events(&[part], "thread-1", "severity", "FPCRM-1")
                .expect_err("wrong node custom event must fail");
        assert!(err.contains("stream node 'classify'"));
    }

    #[test]
    fn require_decision_stream_payload_kind_rejects_missing_registered_debug_projection() {
        let latest = checkpoint(
            "thread-1",
            "checkpoint-1",
            1,
            serde_json::json!({
                "identifier": "FPCRM-1",
                "decision": "Set"
            }),
            "set",
        );
        let stream = vec![
            checkpoint_part(&latest),
            custom_part("set", "severity", "FPCRM-1"),
        ];

        let err = require_decision_stream_payload_kind(&stream, "thread-1", "debug", "v3 debug")
            .expect_err("missing debug projection must fail");
        assert!(err.contains("omitted LangGraph v3 debug payloads"));

        let mut with_debug = stream;
        with_debug.push(debug_part("set"));
        require_decision_stream_payload_kind(&with_debug, "thread-1", "debug", "v3 debug")
            .expect("debug projection evidence passes");
    }

    fn checkpoint(
        thread_id: &str,
        checkpoint_id: &str,
        step_number: u64,
        state: serde_json::Value,
        node_id: &str,
    ) -> DecisionGraphCheckpointSnapshot<serde_json::Value> {
        DecisionGraphCheckpointSnapshot {
            checkpoint_id: checkpoint_id.to_string(),
            parent_checkpoint_id: None,
            thread_id: thread_id.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            step_number,
            source_step: Some(step_number as i32),
            source_type: Some("stream_update".to_string()),
            source_node: Some(node_id.to_string()),
            tags: BTreeMap::new(),
            writes: vec![DecisionGraphCheckpointWriteInfo {
                node_id: node_id.to_string(),
                channel: "state".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
            }],
            state,
        }
    }

    fn checkpoint_part(
        checkpoint: &DecisionGraphCheckpointSnapshot<serde_json::Value>,
    ) -> DecisionGraphStreamPart {
        let source_node = checkpoint.source_node.as_deref().expect("source node");
        DecisionGraphStreamPart {
            stream_protocol: DECISION_STREAM_PROTOCOL.to_string(),
            event_type: "Checkpoint".to_string(),
            node_id: source_node.to_string(),
            timestamp: checkpoint.created_at.clone(),
            superstep: checkpoint.step_number,
            payload_kind: "checkpoints".to_string(),
            payload_json: serde_json::json!({
                "checkpoint_id": checkpoint.checkpoint_id,
                "parent_checkpoint_id": checkpoint.parent_checkpoint_id,
                "thread_id": checkpoint.thread_id,
                "step_number": checkpoint.step_number,
                "source": {
                    "node": source_node,
                    "type": checkpoint.source_type.as_deref().expect("source type"),
                },
                "state": checkpoint.state,
            }),
            subgraph_namespace: Vec::new(),
        }
    }

    fn debug_part(node_id: &str) -> DecisionGraphStreamPart {
        DecisionGraphStreamPart {
            stream_protocol: DECISION_STREAM_PROTOCOL.to_string(),
            event_type: "Debug".to_string(),
            node_id: node_id.to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            superstep: 1,
            payload_kind: "debug".to_string(),
            payload_json: serde_json::json!({
                "node": node_id,
                "status": "complete",
            }),
            subgraph_namespace: Vec::new(),
        }
    }

    fn custom_part(node_id: &str, graph: &str, identifier: &str) -> DecisionGraphStreamPart {
        DecisionGraphStreamPart {
            stream_protocol: DECISION_STREAM_PROTOCOL.to_string(),
            event_type: "Custom".to_string(),
            node_id: node_id.to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            superstep: 1,
            payload_kind: "custom".to_string(),
            payload_json: serde_json::json!({
                "type": "sentinel.decision_node",
                "graph": graph,
                "node_id": node_id,
                "identifier": identifier,
            }),
            subgraph_namespace: Vec::new(),
        }
    }

    fn write_entry(
        checkpoint_id: &str,
        step_number: u64,
        node_id: &str,
        value_json: serde_json::Value,
    ) -> DecisionGraphWriteHistoryEntry {
        DecisionGraphWriteHistoryEntry {
            thread_id: "thread-1".to_string(),
            checkpoint_id: checkpoint_id.to_string(),
            step_number,
            channel: "state".to_string(),
            node_id: node_id.to_string(),
            ts: "2026-01-01T00:00:00Z".to_string(),
            value_len: 1,
            value_sha256: "0".repeat(64),
            value_json,
        }
    }

    fn edge(from: &str, kind: &str, to: Option<&str>) -> DecisionGraphEdgeInfo {
        DecisionGraphEdgeInfo {
            from: from.to_string(),
            kind: kind.to_string(),
            to: to.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn required_checkpointer_backend_requires_every_node_to_match() {
        let nodes = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("sqlite"), Some("database_path::memory:")),
        ];
        assert_eq!(
            required_checkpointer_backend("severity", &nodes).expect("backend"),
            "sqlite"
        );

        let missing = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", None, Some("database_path::memory:")),
        ];
        let err = required_checkpointer_backend("severity", &missing)
            .expect_err("missing backend must fail");
        assert!(err.contains("set"));
        assert!(err.contains("sentinel.checkpointer_backend"));

        let mismatched = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("postgres"), Some("database_path::memory:")),
        ];
        let err = required_checkpointer_backend("severity", &mismatched)
            .expect_err("mismatched backend must fail");
        assert!(err.contains("postgres"));
        assert!(err.contains("sqlite"));
    }

    #[test]
    fn required_checkpointer_scope_requires_every_node_to_match() {
        let nodes = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("sqlite"), Some("database_path::memory:")),
        ];
        assert_eq!(
            required_checkpointer_scope("severity", &nodes).expect("scope"),
            "database_path::memory:"
        );

        let missing = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("sqlite"), None),
        ];
        let err =
            required_checkpointer_scope("severity", &missing).expect_err("missing scope must fail");
        assert!(err.contains("set"));
        assert!(err.contains("sentinel.checkpointer_scope"));

        let mismatched = vec![
            node("classify", Some("sqlite"), Some("database_path:a.db")),
            node("set", Some("sqlite"), Some("database_path:b.db")),
        ];
        let err = required_checkpointer_scope("severity", &mismatched)
            .expect_err("mismatched scope must fail");
        assert!(err.contains("database_path:a.db"));
        assert!(err.contains("database_path:b.db"));
    }

    #[test]
    fn required_checkpointer_tenant_scope_matches_backend_contract() {
        let sqlite_nodes = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("sqlite"), Some("database_path::memory:")),
        ];
        assert_eq!(
            required_checkpointer_tenant_scope("severity", &sqlite_nodes, "sqlite")
                .expect("sqlite tenant scope"),
            None
        );

        let mut redis_nodes = sqlite_nodes.clone();
        for node in &mut redis_nodes {
            node.metadata.insert(
                "sentinel.checkpointer_backend".to_string(),
                "redis".to_string(),
            );
            node.metadata.insert(
                "sentinel.checkpointer_tenant_scope".to_string(),
                "legatus_ai".to_string(),
            );
        }
        assert_eq!(
            required_checkpointer_tenant_scope("severity", &redis_nodes, "redis")
                .expect("redis tenant scope"),
            Some("legatus_ai".to_string())
        );

        let err = required_checkpointer_tenant_scope("severity", &sqlite_nodes, "redis")
            .expect_err("hosted backend must require tenant metadata");
        assert!(err.contains("non-empty sentinel.checkpointer_tenant_scope"));

        let err = required_checkpointer_tenant_scope("severity", &redis_nodes, "sqlite")
            .expect_err("sqlite backend must reject hosted tenant metadata");
        assert!(err.contains("must not carry hosted tenant metadata"));

        let mut mismatched = redis_nodes.clone();
        mismatched[1].metadata.insert(
            "sentinel.checkpointer_tenant_scope".to_string(),
            "other_tenant".to_string(),
        );
        let err = required_checkpointer_tenant_scope("severity", &mismatched, "redis")
            .expect_err("mismatched tenants must fail");
        assert!(err.contains("checkpointer tenant scope"));
        assert!(err.contains("legatus_ai"));
        assert!(err.contains("other_tenant"));
    }

    #[test]
    fn required_decision_schema_contract_requires_langgraph_authority_schemas() {
        let state = Some(serde_json::json!({
            "type": "object",
            "x-sentinel": {
                "graph": "severity",
                "authority": "langgraph"
            }
        }));
        let schema = Some(serde_json::json!({ "type": "object" }));
        required_decision_schema_contract("severity", &state, &schema, &schema)
            .expect("complete LangGraph schema contract passes");

        let err = required_decision_schema_contract("severity", &state, &None, &schema)
            .expect_err("missing input schema must fail");
        assert!(err.contains("input schema"));

        let wrong_graph = Some(serde_json::json!({
            "type": "object",
            "x-sentinel": {
                "graph": "phase",
                "authority": "langgraph"
            }
        }));
        let err = required_decision_schema_contract("severity", &wrong_graph, &schema, &schema)
            .expect_err("wrong graph marker must fail");
        assert!(err.contains("x-sentinel.graph 'phase'"));

        let missing_authority = Some(serde_json::json!({
            "type": "object",
            "x-sentinel": {
                "graph": "severity"
            }
        }));
        let err =
            required_decision_schema_contract("severity", &missing_authority, &schema, &schema)
                .expect_err("missing authority marker must fail");
        assert!(err.contains("x-sentinel.authority"));
    }

    #[test]
    fn required_decision_edge_contract_requires_enterprise_routing_shape() {
        let nodes = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("sqlite"), Some("database_path::memory:")),
            node("skip", Some("sqlite"), Some("database_path::memory:")),
        ];
        let edges = vec![
            edge(START, "normal", Some("classify")),
            edge("classify", "conditional", None),
            edge("set", "normal", Some(END)),
            edge("skip", "normal", Some(END)),
        ];
        required_decision_edge_contract("severity", &nodes, &edges)
            .expect("complete decision edge contract passes");

        let missing_start = vec![
            edge("classify", "conditional", None),
            edge("set", "normal", Some(END)),
            edge("skip", "normal", Some(END)),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &missing_start)
            .expect_err("missing START routing must fail");
        assert!(err.contains("missing START routing"));

        let duplicate_start = vec![
            edge(START, "normal", Some("classify")),
            edge(START, "normal", Some("set")),
            edge("classify", "conditional", None),
            edge("set", "normal", Some(END)),
            edge("skip", "normal", Some(END)),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &duplicate_start)
            .expect_err("duplicate START routing must fail");
        assert!(err.contains("duplicate START routing"));

        let no_conditional = vec![
            edge(START, "normal", Some("classify")),
            edge("classify", "normal", Some("set")),
            edge("set", "normal", Some(END)),
            edge("skip", "normal", Some(END)),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &no_conditional)
            .expect_err("missing conditional decision edge must fail");
        assert!(err.contains("conditional decision edge"));

        let no_terminal = vec![
            edge(START, "normal", Some("classify")),
            edge("classify", "conditional", None),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &no_terminal)
            .expect_err("missing terminal edge must fail");
        assert!(err.contains("terminal edge to END"));

        let unexpected_source = vec![
            edge(START, "normal", Some("classify")),
            edge("classify", "conditional", None),
            edge("set", "normal", Some(END)),
            edge("skip", "normal", Some(END)),
            edge("ghost", "normal", Some(END)),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &unexpected_source)
            .expect_err("unexpected source must fail");
        assert!(err.contains("unexpected LangGraph edge source"));

        let unexpected_target = vec![
            edge(START, "normal", Some("classify")),
            edge("classify", "conditional", None),
            edge("set", "normal", Some("ghost")),
            edge("skip", "normal", Some(END)),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &unexpected_target)
            .expect_err("unexpected target must fail");
        assert!(err.contains("targets unexpected node"));

        let missing_target = vec![
            edge(START, "normal", Some("classify")),
            edge("classify", "conditional", None),
            edge("set", "normal", None),
            edge("skip", "normal", Some(END)),
        ];
        let err = required_decision_edge_contract("severity", &nodes, &missing_target)
            .expect_err("non-conditional edge without target must fail");
        assert!(err.contains("missing a target"));
    }

    #[test]
    fn required_decision_runtime_contract_requires_durable_auto_checkpointed_execution() {
        required_decision_runtime_contract("severity", true, true, 100, 3)
            .expect("durable auto-checkpointed decision runtime passes");

        let err = required_decision_runtime_contract("severity", false, true, 100, 3)
            .expect_err("missing checkpointer must fail");
        assert!(err.contains("durable LangGraph checkpointer"));

        let err = required_decision_runtime_contract("severity", true, false, 100, 3)
            .expect_err("disabled auto-checkpointing must fail");
        assert!(err.contains("auto-checkpointing"));

        let err = required_decision_runtime_contract("severity", true, true, 3, 3)
            .expect_err("insufficient iteration headroom must fail");
        assert!(err.contains("max_iterations 3"));
    }

    #[test]
    fn required_decision_node_runtime_contract_requires_enterprise_node_configuration() {
        let nodes = vec![
            node("classify", Some("sqlite"), Some("database_path::memory:")),
            node("set", Some("sqlite"), Some("database_path::memory:")),
        ];
        required_decision_node_runtime_contract("severity", &nodes)
            .expect("fully configured decision nodes pass");

        let mut wrong_graph = nodes.clone();
        wrong_graph[0]
            .metadata
            .insert("sentinel.graph".to_string(), "phase".to_string());
        let err = required_decision_node_runtime_contract("severity", &wrong_graph)
            .expect_err("wrong graph metadata must fail");
        assert!(err.contains("graph 'phase'"));
        assert!(err.contains("severity"));

        let mut missing_node = nodes.clone();
        missing_node[1].metadata.remove("sentinel.node");
        let err = required_decision_node_runtime_contract("severity", &missing_node)
            .expect_err("missing node metadata must fail");
        assert!(err.contains("sentinel.node"));
        assert!(err.contains("set"));

        let mut missing_timeout = nodes;
        missing_timeout[0].has_timeout_policy = false;
        let err = required_decision_node_runtime_contract("severity", &missing_timeout)
            .expect_err("missing timeout must fail");
        assert!(err.contains("timeout policy"));
    }

    #[tokio::test]
    async fn drain_v3_messages_records_all_chat_model_subchannels() {
        use std::collections::HashMap;

        use langgraph_core::domain::value_objects::{Message, TokenUsage, ToolCall};

        let (stream, mut senders) = ChatModelStream::channels("llm_node");
        senders.dispatch_text_delta("hello").await;
        senders.dispatch_reasoning_delta("thinking").await;
        senders
            .dispatch_tool_call(ToolCall::new(
                "call_1",
                "lookup",
                HashMap::from([("query".to_string(), serde_json::json!("rust"))]),
            ))
            .await;
        senders.dispatch_usage(TokenUsage::new(5, 7, 12)).await;
        senders.finalize(Message::assistant("done"));
        drop(senders);

        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(stream).await.expect("send chat stream");
        drop(tx);

        let drain = drain_v3_messages(rx).await.expect("message drain");
        let events: Vec<_> = drain
            .stream
            .iter()
            .map(|part| part.event_type.as_str())
            .collect();
        for expected in [
            "MessageStreamStarted",
            "MessageTextDelta",
            "MessageReasoningDelta",
            "MessageToolCall",
            "MessageUsage",
            "MessageOutput",
        ] {
            assert!(
                events.contains(&expected),
                "missing {expected} in {events:?}"
            );
        }

        assert!(drain.stream.iter().any(|part| {
            part.event_type == "MessageTextDelta" && part.payload_json["delta"] == "hello"
        }));
        assert!(drain.stream.iter().any(|part| {
            part.event_type == "MessageReasoningDelta" && part.payload_json["delta"] == "thinking"
        }));
        assert!(drain.stream.iter().any(|part| {
            part.event_type == "MessageToolCall"
                && part.payload_json["tool_call"]["name"] == "lookup"
        }));
        assert!(drain.stream.iter().any(|part| {
            part.event_type == "MessageUsage" && part.payload_json["usage"]["total_tokens"] == 12
        }));
        assert!(drain.stream.iter().any(|part| {
            part.event_type == "MessageOutput" && part.payload_json["message"]["content"] == "done"
        }));
    }
}
