//! # sentinel-graph
//!
//! Phase-progression engine for sentinel skills, powered by
//! [`langgraph-core`](../langgraph_core/index.html) — the Rust port of
//! `LangGraph` (Python 1.2.0 parity).
//!
//! ## Why a graph
//!
//! A sentinel skill workflow is an *ordered sequence of verifiable phases*,
//! each gated by an AI judge that must pass before the next phase unlocks.
//! That is exactly `LangGraph`'s canonical **workflow** pattern: a Generator
//! (the phase) feeding an Evaluator (the judge) with a **loop-back on
//! failure**. Modelling it as a `StateGraph` buys us, for free, the features
//! that sentinel otherwise hand-rolls:
//!
//! - **Conditional routing** — Pass advances to the next phase, Fail loops
//!   back to the same phase (`add_conditional_edge`).
//! - **Durable, cross-process state** — sentinel hooks are fresh
//!   ms-cold-start processes; the graph advances *one phase per async MCP
//!   call* and checkpoints to a sqlite [`CheckpointSaver`] keyed by a
//!   skill-scoped thread id (`sentinel.phase.<skill>.<session_id>` for local
//!   SQLite, prefixed by `tenant:<id>:` in hosted distributed-checkpointer mode). Process
//!   death between calls is fine: `load_latest` restores. This is `LangGraph`'s
//!   durable-execution model and it is a near-exact match for sentinel's
//!   invocation model.
//! - **Judge-as-interrupt** — every phase node interrupts *after* execution
//!   (`InterruptConfig::after()`); the out-of-process judge verdict is written
//!   as a graph checkpoint and execution is re-invoked on the same thread so
//!   the conditional edge structurally routes Pass/Fail.
//! - **Time-travel** — a QA-failed re-attempt forks from the checkpoint
//!   *before* the failed phase (`get_state_history` + `update_state`).
//!
//! ## Boundary
//!
//! This crate owns the langgraph dependency. The **async MCP server** advances
//! the graph, while the hook process reads durable checkpoints to project the
//! graph-owned state before enforcing `phase_gate`. The graph's state type
//! ([`PhaseGraphState`]) is graph-local and converts to/from the domain
//! [`WorkflowState`], keeping `sentinel-domain` free of any langgraph dependency
//! (hexagonal boundary).
//!
//! [`CheckpointSaver`]: langgraph_core::ports::CheckpointSaver
//! [`WorkflowState`]: sentinel_domain::workflow::WorkflowState

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use langgraph_core::application::services::{
    get_stream_writer, ChatModelStream, CheckpointEvent, CheckpointStateSnapshot,
    CheckpointsTransformer, CompilationResult, CompiledGraphV3Ext, CustomEvent, CustomTransformer,
    DebugEvent, DebugTransformer, GraphCompiler, GraphRunStream, LifecycleEvent, Projection,
    RunV3Options, StateSnapshot, StateUpdateEvent, SubgraphHandle, TaskEvent, TasksTransformer,
    UpdatesTransformer, V3StreamTransformer,
};
use langgraph_core::domain::value_objects::{
    Command, Interrupt, InterruptConfig, NodeConfig, NodeError, NodeErrorContext,
    NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::ports::CheckpointSaver;
#[cfg(feature = "postgres")]
use langgraph_core::PostgresCheckpointer;
#[cfg(feature = "redis")]
use langgraph_core::RedisCheckpointer;
#[cfg(feature = "sqlite")]
use langgraph_core::SqliteCheckpointer;
use langgraph_core::StateGraphBuilder;

use sentinel_domain::workflow::{
    DyadVerdicts, SkillWorkflow, StepState, StepStatus, WorkflowState, WorkflowStep,
};

mod error;
pub use error::GraphEngineError;

#[cfg(test)]
mod tests;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, GraphEngineError>;

const PHASE_GRAPH_NAME: &str = "phase";
const PHASE_STREAM_PROTOCOL: &str = "v3";
const STATE_SNAPSHOT_NODE: &str = "__state__";
const CHECKPOINTER_BACKEND_METADATA: &str = "sentinel.checkpointer_backend";
const CHECKPOINTER_SCOPE_METADATA: &str = "sentinel.checkpointer_scope";
const CHECKPOINTER_TENANT_SCOPE_METADATA: &str = "sentinel.checkpointer_tenant_scope";
pub const LANGGRAPH_TENANT_ENV: &str = sentinel_domain::langgraph_thread::LANGGRAPH_TENANT_ENV;

fn tenant_scope_from_env() -> Result<Option<String>> {
    let value = match std::env::var(LANGGRAPH_TENANT_ENV) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => {
            return Err(GraphEngineError::Checkpointer(format!(
                "failed to read {LANGGRAPH_TENANT_ENV}: {err}"
            )));
        }
    };
    let tenant = value.trim();
    if tenant.is_empty() {
        return Err(GraphEngineError::Checkpointer(format!(
            "{LANGGRAPH_TENANT_ENV} is set but empty"
        )));
    }
    sentinel_domain::langgraph_thread::validate_tenant_scope(tenant)
        .map_err(GraphEngineError::Checkpointer)?;
    Ok(Some(tenant.to_string()))
}

/// The verdict a judge returns for a phase. Serializable so it survives the
/// checkpoint round-trip: the phase graph authority records it, then the
/// conditional edge reads it on checkpoint-backed re-invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// No verdict yet — the graph is paused at the post-phase interrupt
    /// waiting for the out-of-process judge.
    #[default]
    Pending,
    /// Evidence sufficient — advance to the next phase.
    Pass,
    /// Evidence insufficient — loop back and re-run the phase.
    Fail,
}

/// Graph-local execution state. Mirrors the relevant [`WorkflowState`] fields
/// plus the in-flight [`Verdict`]. Kept separate from the domain type so
/// `sentinel-domain` carries no langgraph dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphState {
    /// Skill this graph is executing.
    pub skill: String,
    /// Session id — also the checkpointer `thread_id`.
    pub session_id: String,
    /// Ordered phase ids, captured at compile time so nodes can route by
    /// position without re-reading the workflow config.
    pub phase_order: Vec<String>,
    /// Index of the phase currently being evaluated (`None` = not started).
    pub current_phase: Option<usize>,
    /// Phase ids completed with a passing verdict, in order.
    pub completed_phases: Vec<String>,
    /// Whether all phases have passed.
    pub complete: bool,
    /// Role-dyad verifier state mirrored from [`WorkflowState`]. Persisting it
    /// in graph checkpoints lets the LangGraph authority enforce reviewer/tester
    /// requirements instead of relying on direct `WorkflowState` mutation.
    #[serde(default)]
    pub dyad_verdicts: std::collections::BTreeMap<String, DyadVerdicts>,
    /// Step-level status tracked through the same durable checkpoint timeline
    /// as phase progress.
    #[serde(default)]
    pub step_states: Vec<StepState>,
    /// Runtime policy captured from the configured [`WorkflowStep`] at the
    /// moment a step-status mutation is checkpointed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_policy_evidence: Vec<StepRuntimePolicyEvidence>,
    /// Currently active step ID, mirrored from [`WorkflowState`].
    #[serde(default)]
    pub current_step: Option<String>,
    /// Replay/time-travel audit events persisted in the checkpoint timeline.
    /// Each event records the operator reason and the graph-owned progress that
    /// was superseded by the fork.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replay_events: Vec<PhaseReplayEvent>,
    /// Node-level compensation events emitted by LangGraph error handlers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phase_error_events: Vec<PhaseGraphErrorEvent>,
    /// The verdict for the phase at `current_phase`. Set before durable
    /// re-invocation, then read by the conditional edge.
    pub last_verdict: Verdict,
}

/// Durable node-level error compensation metadata attached to
/// [`PhaseGraphState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphErrorEvent {
    /// Phase that owned the failing node.
    pub phase_id: String,
    /// LangGraph node id reported by [`NodeErrorContext`].
    pub node_id: String,
    /// Display form of the underlying LangGraph error.
    pub error: String,
}

/// Checkpointed runtime policy for a step-status mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRuntimePolicyEvidence {
    pub phase_id: String,
    pub step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub retry_max_attempts: u32,
    pub retry_backoff_ms: u64,
    pub retry_on: Vec<String>,
    pub circuit_failure_threshold: u32,
    pub circuit_cooldown_ms: u64,
}

impl StepRuntimePolicyEvidence {
    #[must_use]
    pub fn from_workflow_step(phase_id: impl Into<String>, step: &WorkflowStep) -> Self {
        Self {
            phase_id: phase_id.into(),
            step_id: step.id.clone(),
            timeout_ms: step.timeout_ms,
            retry_max_attempts: step.retry_policy.max_attempts,
            retry_backoff_ms: step.retry_policy.backoff_ms,
            retry_on: step.retry_policy.retry_on.clone(),
            circuit_failure_threshold: step.circuit_breaker.failure_threshold,
            circuit_cooldown_ms: step.circuit_breaker.cooldown_ms,
        }
    }
}

/// Durable replay/time-travel audit metadata attached to [`PhaseGraphState`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseReplayEvent {
    /// Phase id targeted by the replay.
    pub phase_id: String,
    /// Non-empty operator reason for forking the graph history.
    pub reason: String,
    /// Completed phases at or after the replay point that the fork superseded.
    pub superseded_completed_phases: Vec<String>,
    /// Step states at or after the replay point that the fork superseded.
    pub superseded_step_states: Vec<PhaseReplayStepState>,
}

/// Lightweight replay snapshot of a superseded step. This avoids embedding the
/// full domain step type again inside replay history while preserving the audit
/// fields operators need to understand what the fork invalidated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseReplayStepState {
    pub step_id: String,
    pub phase_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl From<&StepState> for PhaseReplayStepState {
    fn from(step: &StepState) -> Self {
        Self {
            step_id: step.step_id.clone(),
            phase_id: step.phase_id.clone(),
            status: step_status_name(&step.status).to_string(),
            started_at: step.started_at.map(|ts| ts.to_rfc3339()),
            completed_at: step.completed_at.map(|ts| ts.to_rfc3339()),
            summary: step.summary.clone(),
        }
    }
}

/// Serializable view of one compiled LangGraph node used by Sentinel's phase
/// engine. This is runtime graph metadata, not the TOML workflow definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphNodeInfo {
    pub id: String,
    pub deferred: bool,
    pub barrier_on: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub has_error_handler: bool,
    pub has_timeout_policy: bool,
    pub interrupt_before: bool,
    pub interrupt_after: bool,
}

/// Serializable view of one compiled LangGraph edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphEdgeInfo {
    pub from: String,
    pub kind: String,
    pub to: Option<String>,
}

/// JSON schema contracts preserved by the compiled LangGraph runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphSchemas {
    pub state: Option<serde_json::Value>,
    pub input: Option<serde_json::Value>,
    pub output: Option<serde_json::Value>,
    pub context: Option<serde_json::Value>,
}

/// Runtime topology for the compiled, checkpointed phase graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphIntrospection {
    pub skill: String,
    pub thread_id: String,
    pub phase_order: Vec<String>,
    pub durable_checkpointer: bool,
    pub checkpointer_backend: String,
    pub checkpointer_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpointer_tenant_scope: Option<String>,
    pub auto_checkpoint: bool,
    pub max_iterations: usize,
    pub schemas: PhaseGraphSchemas,
    pub nodes: Vec<PhaseGraphNodeInfo>,
    pub edges: Vec<PhaseGraphEdgeInfo>,
}

/// Serializable view of one LangGraph stream part emitted during phase execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseGraphStreamPart {
    pub stream_protocol: String,
    pub event_type: String,
    pub node_id: String,
    pub timestamp: String,
    pub superstep: u64,
    pub payload_kind: String,
    pub payload_json: serde_json::Value,
    pub subgraph_namespace: Vec<String>,
}

/// State plus the exact LangGraph stream parts observed during a gate run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseGraphRunReport {
    pub state: PhaseGraphState,
    pub stream: Vec<PhaseGraphStreamPart>,
}

struct PhaseV3StreamRun {
    output: PhaseGraphState,
    interrupted: bool,
    interrupts: Vec<Interrupt>,
    stream: Vec<PhaseGraphStreamPart>,
}

#[derive(Default)]
struct V3PhaseProjectionDrain {
    stream: Vec<PhaseGraphStreamPart>,
    node_error: Option<String>,
}

impl V3PhaseProjectionDrain {
    fn push(&mut self, part: PhaseGraphStreamPart) {
        if part.payload_kind == "tasks"
            && part
                .payload_json
                .get("status")
                .and_then(serde_json::Value::as_str)
                == Some("error")
        {
            self.node_error = Some(phase_task_error_message(&part));
        }
        if part.event_type == "SubgraphFailed" {
            self.node_error = Some(phase_lifecycle_error_message(&part));
        }
        self.stream.push(part);
    }

    fn extend(&mut self, other: Self) {
        if self.node_error.is_none() {
            self.node_error = other.node_error;
        }
        self.stream.extend(other.stream);
    }
}

fn phase_task_error_message(part: &PhaseGraphStreamPart) -> String {
    part.payload_json
        .get("error")
        .and_then(serde_json::Value::as_str)
        .filter(|error| !error.trim().is_empty())
        .map(|error| format!("phase stream failed at node {}: {error}", part.node_id))
        .unwrap_or_else(|| format!("phase stream failed at node {}", part.node_id))
}

fn phase_lifecycle_error_message(part: &PhaseGraphStreamPart) -> String {
    part.payload_json
        .get("error")
        .and_then(serde_json::Value::as_str)
        .filter(|error| !error.trim().is_empty())
        .map(|error| {
            format!(
                "phase stream lifecycle failed at node {}: {error}",
                part.node_id
            )
        })
        .unwrap_or_else(|| format!("phase stream lifecycle failed at node {}", part.node_id))
}

/// Serializable checkpoint source metadata from the LangGraph runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphCheckpointSource {
    pub step: i32,
    pub source_type: String,
    pub node: Option<String>,
}

/// Serializable checkpoint write metadata from the LangGraph runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphCheckpointWrite {
    pub node_id: String,
    pub channel: String,
    pub ts: String,
}

/// Durable LangGraph checkpoint snapshot plus the reconstructed phase state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphCheckpointSnapshot {
    pub checkpoint_id: String,
    pub parent_checkpoint_id: Option<String>,
    pub thread_id: String,
    pub step_number: u64,
    pub created_at: String,
    pub tags: BTreeMap<String, String>,
    pub source: Option<PhaseGraphCheckpointSource>,
    pub writes: Vec<PhaseGraphCheckpointWrite>,
    pub state: PhaseGraphState,
}

/// Per-write history entry from LangGraph's checkpoint write stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGraphWriteHistoryEntry {
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

impl PhaseGraphState {
    /// Initial state for a skill's workflow run.
    #[must_use]
    pub fn new(
        skill: impl Into<String>,
        session_id: impl Into<String>,
        phase_order: Vec<String>,
    ) -> Self {
        Self {
            skill: skill.into(),
            session_id: session_id.into(),
            phase_order,
            current_phase: None,
            completed_phases: Vec::new(),
            complete: false,
            dyad_verdicts: std::collections::BTreeMap::new(),
            step_states: Vec::new(),
            step_policy_evidence: Vec::new(),
            current_step: None,
            replay_events: Vec::new(),
            phase_error_events: Vec::new(),
            last_verdict: Verdict::Pending,
        }
    }

    /// Project this graph state onto the domain [`WorkflowState`] that the
    /// rest of sentinel (and the sync `phase_gate`) reads.
    #[must_use]
    pub fn to_workflow_state(&self) -> WorkflowState {
        let mut ws = WorkflowState::new(self.skill.clone(), self.session_id.clone());
        ws.current_phase = self.current_phase;
        ws.completed_phases.clone_from(&self.completed_phases);
        ws.complete = self.complete;
        ws.dyad_verdicts.clone_from(&self.dyad_verdicts);
        ws.step_states.clone_from(&self.step_states);
        ws.current_step.clone_from(&self.current_step);
        ws
    }

    /// Hydrate graph state from a domain [`WorkflowState`] plus the phase order
    /// from the workflow definition for projection round-trip tests only.
    #[must_use]
    #[cfg(test)]
    fn from_workflow_state(ws: &WorkflowState, phase_order: Vec<String>) -> Self {
        Self {
            skill: ws.skill.clone(),
            session_id: ws.session_id.clone(),
            phase_order,
            current_phase: ws.current_phase,
            completed_phases: ws.completed_phases.clone(),
            complete: ws.complete,
            dyad_verdicts: ws.dyad_verdicts.clone(),
            step_states: ws.step_states.clone(),
            step_policy_evidence: Vec::new(),
            current_step: ws.current_step.clone(),
            replay_events: Vec::new(),
            phase_error_events: Vec::new(),
            last_verdict: Verdict::Pending,
        }
    }
}

/// Runtime backend selector for durable phase-graph checkpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseCheckpointerConfig {
    /// Durable local SQLite database path.
    Sqlite { database_path: String },
    /// Durable Postgres database URL plus explicit schema.
    Postgres {
        database_url: String,
        schema: String,
    },
    /// Durable Redis connection URL plus optional checkpoint TTL.
    Redis {
        redis_url: String,
        ttl_seconds: Option<u64>,
    },
}

/// A LangGraph checkpointer plus the backend identity used to build it.
#[derive(Clone)]
pub struct PhaseCheckpointer {
    saver: Arc<dyn CheckpointSaver>,
    backend: &'static str,
    scope: String,
    tenant_scope: Option<String>,
}

impl PhaseCheckpointer {
    #[must_use]
    pub fn backend(&self) -> &'static str {
        self.backend
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub fn tenant_scope(&self) -> Option<&str> {
        self.tenant_scope.as_deref()
    }

    #[must_use]
    pub fn tenant_scope_metadata_value(&self) -> &str {
        self.tenant_scope().unwrap_or("")
    }

    #[must_use]
    pub fn saver(&self) -> Arc<dyn CheckpointSaver> {
        Arc::clone(&self.saver)
    }

    #[must_use]
    pub fn into_saver(self) -> Arc<dyn CheckpointSaver> {
        self.saver
    }
}

impl PhaseCheckpointerConfig {
    /// Env var selecting the checkpoint backend.
    pub const BACKEND_ENV: &'static str = "SENTINEL_PHASE_GRAPH_CHECKPOINTER";
    /// Env var providing the Postgres database URL when backend is `postgres`.
    pub const POSTGRES_URL_ENV: &'static str = "SENTINEL_PHASE_GRAPH_POSTGRES_URL";
    /// Required schema for Postgres checkpoints.
    pub const POSTGRES_SCHEMA_ENV: &'static str = "SENTINEL_PHASE_GRAPH_POSTGRES_SCHEMA";
    /// Env var providing the Redis URL when backend is `redis`.
    pub const REDIS_URL_ENV: &'static str = "SENTINEL_PHASE_GRAPH_REDIS_URL";
    /// Optional Redis checkpoint TTL in seconds.
    pub const REDIS_TTL_SECS_ENV: &'static str = "SENTINEL_PHASE_GRAPH_REDIS_TTL_SECS";

    /// Build config from process environment.
    ///
    /// No backend variable means the caller-provided SQLite path is the explicit
    /// local default. If `postgres` or `redis` is selected, the backend URL and tenant
    /// scope are mandatory; Sentinel refuses to switch to SQLite after an
    /// enterprise backend is requested.
    pub fn from_env(sqlite_database_path: impl Into<String>) -> Result<Self> {
        let backend = phase_checkpointer_backend_from_env()?;
        match backend.as_str() {
            "sqlite" => Ok(Self::Sqlite {
                database_path: sqlite_database_path.into(),
            }),
            "postgres" => {
                let database_url = std::env::var(Self::POSTGRES_URL_ENV)
                    .map_err(|_| {
                        GraphEngineError::Checkpointer(format!(
                            "{}=postgres requires {}",
                            Self::BACKEND_ENV,
                            Self::POSTGRES_URL_ENV
                        ))
                    })?
                    .trim()
                    .to_string();
                if database_url.is_empty() {
                    return Err(GraphEngineError::Checkpointer(format!(
                        "{}=postgres requires non-empty {}",
                        Self::BACKEND_ENV,
                        Self::POSTGRES_URL_ENV
                    )));
                }
                let schema = required_non_empty_env(Self::POSTGRES_SCHEMA_ENV, || {
                    format!(
                        "{}=postgres requires {}",
                        Self::BACKEND_ENV,
                        Self::POSTGRES_SCHEMA_ENV
                    )
                })?;
                require_enterprise_tenant_scope(Self::BACKEND_ENV, "postgres")?;
                Ok(Self::Postgres {
                    database_url,
                    schema,
                })
            }
            "redis" => {
                let redis_url = std::env::var(Self::REDIS_URL_ENV)
                    .map_err(|_| {
                        GraphEngineError::Checkpointer(format!(
                            "{}=redis requires {}",
                            Self::BACKEND_ENV,
                            Self::REDIS_URL_ENV
                        ))
                    })?
                    .trim()
                    .to_string();
                if redis_url.is_empty() {
                    return Err(GraphEngineError::Checkpointer(format!(
                        "{}=redis requires non-empty {}",
                        Self::BACKEND_ENV,
                        Self::REDIS_URL_ENV
                    )));
                }
                let ttl_seconds = optional_positive_u64_env(Self::REDIS_TTL_SECS_ENV)?;
                require_enterprise_tenant_scope(Self::BACKEND_ENV, "redis")?;
                Ok(Self::Redis {
                    redis_url,
                    ttl_seconds,
                })
            }
            _ => unreachable!("phase_checkpointer_backend_from_env only returns known backends"),
        }
    }

    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Sqlite { .. } => "sqlite",
            Self::Postgres { .. } => "postgres",
            Self::Redis { .. } => "redis",
        }
    }

    #[must_use]
    pub fn scope_name(&self) -> String {
        match self {
            Self::Sqlite { database_path } => format!("database_path:{database_path}"),
            Self::Postgres { schema, .. } => format!("schema:{schema}"),
            Self::Redis { ttl_seconds, .. } => match ttl_seconds {
                Some(ttl_seconds) => format!("ttl_seconds:{ttl_seconds}"),
                None => "ttl_seconds:none".to_string(),
            },
        }
    }
}

fn phase_checkpointer_backend_from_env() -> Result<String> {
    let backend = match std::env::var(PhaseCheckpointerConfig::BACKEND_ENV) {
        Ok(value) => {
            let backend = value.trim();
            if backend.is_empty() {
                return Err(GraphEngineError::Checkpointer(format!(
                    "{} is set but empty; expected sqlite, postgres, or redis",
                    PhaseCheckpointerConfig::BACKEND_ENV
                )));
            }
            backend.to_ascii_lowercase()
        }
        Err(std::env::VarError::NotPresent) => return Ok("sqlite".to_string()),
        Err(err) => {
            return Err(GraphEngineError::Checkpointer(format!(
                "failed to read {}: {err}",
                PhaseCheckpointerConfig::BACKEND_ENV
            )));
        }
    };

    match backend.as_str() {
        "sqlite" => Ok("sqlite".to_string()),
        "postgres" => Ok("postgres".to_string()),
        "redis" => Ok("redis".to_string()),
        other => Err(GraphEngineError::Checkpointer(format!(
            "unsupported phase graph checkpointer backend '{other}' from {}; expected sqlite, postgres, or redis",
            PhaseCheckpointerConfig::BACKEND_ENV
        ))),
    }
}

fn optional_non_empty_env(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err(GraphEngineError::Checkpointer(format!(
                    "{name} is set but empty"
                )));
            }
            Ok(Some(value.to_string()))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(GraphEngineError::Checkpointer(format!(
            "failed to read {name}: {err}"
        ))),
    }
}

fn required_non_empty_env(name: &str, missing: impl FnOnce() -> String) -> Result<String> {
    match optional_non_empty_env(name)? {
        Some(value) => Ok(value),
        None => Err(GraphEngineError::Checkpointer(missing())),
    }
}

fn optional_positive_u64_env(name: &str) -> Result<Option<u64>> {
    let value = match std::env::var(name) {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(err) => {
            return Err(GraphEngineError::Checkpointer(format!(
                "failed to read {name}: {err}"
            )));
        }
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(GraphEngineError::Checkpointer(format!(
            "{name} is set but empty"
        )));
    }
    let parsed = value.parse::<u64>().map_err(|err| {
        GraphEngineError::Checkpointer(format!(
            "{name} must be a positive integer number of seconds: {err}"
        ))
    })?;
    if parsed == 0 {
        return Err(GraphEngineError::Checkpointer(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(Some(parsed))
}

fn require_enterprise_tenant_scope(backend_env: &str, backend: &str) -> Result<()> {
    if tenant_scope_from_env()?.is_some() {
        return Ok(());
    }

    Err(GraphEngineError::Checkpointer(format!(
        "{backend_env}={backend} requires {LANGGRAPH_TENANT_ENV} so LangGraph checkpoint thread_id values are tenant-scoped"
    )))
}

#[cfg(any(feature = "postgres", feature = "redis", test))]
fn tenant_scope_for_checkpointer_backend(backend: &str) -> Result<Option<String>> {
    match backend {
        "sqlite" => Ok(None),
        "postgres" | "redis" => tenant_scope_from_env()?.map_or_else(
            || {
                Err(GraphEngineError::Checkpointer(format!(
                    "phase graph {backend} checkpointer requires {LANGGRAPH_TENANT_ENV} so LangGraph checkpoint thread_id values are tenant-scoped"
                )))
            },
            |tenant| Ok(Some(tenant)),
        ),
        other => Err(GraphEngineError::Checkpointer(format!(
            "unsupported phase graph checkpointer backend '{other}'"
        ))),
    }
}

fn tenant_scope_for_phase_checkpointer_config(
    config: &PhaseCheckpointerConfig,
) -> Result<Option<String>> {
    match config {
        PhaseCheckpointerConfig::Sqlite { .. } => Ok(None),
        PhaseCheckpointerConfig::Postgres { .. } => {
            tenant_scope_for_postgres_phase_checkpointer_config()
        }
        PhaseCheckpointerConfig::Redis { .. } => tenant_scope_for_redis_phase_checkpointer_config(),
    }
}

#[cfg(feature = "postgres")]
fn tenant_scope_for_postgres_phase_checkpointer_config() -> Result<Option<String>> {
    tenant_scope_for_checkpointer_backend("postgres")
}

#[cfg(not(feature = "postgres"))]
fn tenant_scope_for_postgres_phase_checkpointer_config() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(feature = "redis")]
fn tenant_scope_for_redis_phase_checkpointer_config() -> Result<Option<String>> {
    tenant_scope_for_checkpointer_backend("redis")
}

#[cfg(not(feature = "redis"))]
fn tenant_scope_for_redis_phase_checkpointer_config() -> Result<Option<String>> {
    Ok(None)
}

fn validate_phase_checkpointer_tenant_scope(
    backend: &str,
    tenant_scope: Option<&str>,
) -> Result<()> {
    match backend {
        "sqlite" => {
            if tenant_scope.is_some() {
                Err(GraphEngineError::Checkpointer(
                    "SQLite phase graph must not carry hosted tenant metadata".into(),
                ))
            } else {
                Ok(())
            }
        }
        "postgres" | "redis" => {
            let tenant = tenant_scope.ok_or_else(|| {
                GraphEngineError::Checkpointer(format!(
                    "phase graph {backend} checkpointer requires {LANGGRAPH_TENANT_ENV} so LangGraph checkpoint thread_id values are tenant-scoped"
                ))
            })?;
            sentinel_domain::langgraph_thread::validate_tenant_scope(tenant)
                .map_err(GraphEngineError::Checkpointer)?;
            Ok(())
        }
        other => Err(GraphEngineError::Checkpointer(format!(
            "unsupported phase graph checkpointer backend '{other}'"
        ))),
    }
}

/// Build a durable sqlite [`CheckpointSaver`] for a session. The database path
/// is the caller's responsibility (it lives under sentinel's state dir); pass
/// `":memory:"` for tests.
///
/// # Errors
/// Returns [`GraphEngineError::Checkpointer`] if the sqlite pool/schema cannot
/// be initialised.
async fn phase_saver(database_path: &str) -> Result<Arc<dyn CheckpointSaver>> {
    phase_saver_sqlite(database_path).await
}

#[cfg(feature = "sqlite")]
async fn phase_saver_sqlite(database_path: &str) -> Result<Arc<dyn CheckpointSaver>> {
    let saver = SqliteCheckpointer::new(database_path)
        .await
        .map_err(GraphEngineError::from_graph)?;
    Ok(Arc::new(saver))
}

#[cfg(not(feature = "sqlite"))]
async fn phase_saver_sqlite(_database_path: &str) -> Result<Arc<dyn CheckpointSaver>> {
    Err(GraphEngineError::Checkpointer(
        "phase graph SQLite checkpointer requested, but sentinel-graph was built without the sqlite feature".into(),
    ))
}

/// Build a durable SQLite phase-graph checkpointer with backend identity.
pub async fn phase_checkpointer(database_path: &str) -> Result<PhaseCheckpointer> {
    Ok(PhaseCheckpointer {
        saver: phase_saver(database_path).await?,
        backend: "sqlite",
        scope: format!("database_path:{database_path}"),
        tenant_scope: None,
    })
}

/// Build a durable phase-graph checkpointer from an explicit config.
///
/// If a selected backend is missing from the build features, this errors. It
/// never silently switches to SQLite.
pub async fn phase_checkpointer_for_config(
    config: PhaseCheckpointerConfig,
) -> Result<PhaseCheckpointer> {
    let backend = config.backend_name();
    let scope = config.scope_name();
    let tenant_scope = tenant_scope_for_phase_checkpointer_config(&config)?;
    let saver = match config {
        PhaseCheckpointerConfig::Sqlite { database_path } => phase_saver(&database_path).await,
        PhaseCheckpointerConfig::Postgres {
            database_url,
            schema,
        } => phase_saver_postgres(&database_url, &schema).await,
        PhaseCheckpointerConfig::Redis {
            redis_url,
            ttl_seconds,
        } => phase_saver_redis(&redis_url, ttl_seconds).await,
    }?;
    Ok(PhaseCheckpointer {
        saver,
        backend,
        scope,
        tenant_scope,
    })
}

/// Build the runtime phase-graph checkpointer using env-selected backend.
///
/// `sqlite_database_path` is used only when the backend is unset or explicitly
/// `sqlite`.
pub async fn phase_checkpointer_from_env(sqlite_database_path: &str) -> Result<PhaseCheckpointer> {
    let config = PhaseCheckpointerConfig::from_env(sqlite_database_path.to_string())?;
    phase_checkpointer_for_config(config).await
}

#[cfg(feature = "postgres")]
async fn phase_saver_postgres(
    database_url: &str,
    schema: &str,
) -> Result<Arc<dyn CheckpointSaver>> {
    let saver = PostgresCheckpointer::with_schema(database_url, schema)
        .await
        .map_err(GraphEngineError::from_graph)?;
    Ok(Arc::new(saver))
}

#[cfg(not(feature = "postgres"))]
async fn phase_saver_postgres(
    _database_url: &str,
    _schema: &str,
) -> Result<Arc<dyn CheckpointSaver>> {
    Err(GraphEngineError::Checkpointer(
        "phase graph Postgres checkpointer requested, but sentinel-graph was built without the postgres feature".into(),
    ))
}

#[cfg(feature = "redis")]
async fn phase_saver_redis(
    redis_url: &str,
    ttl_seconds: Option<u64>,
) -> Result<Arc<dyn CheckpointSaver>> {
    let saver = match ttl_seconds {
        Some(ttl_seconds) => RedisCheckpointer::with_ttl(redis_url, ttl_seconds).await,
        None => RedisCheckpointer::new(redis_url).await,
    }
    .map_err(GraphEngineError::from_graph)?;
    Ok(Arc::new(saver))
}

#[cfg(not(feature = "redis"))]
async fn phase_saver_redis(
    _redis_url: &str,
    _ttl_seconds: Option<u64>,
) -> Result<Arc<dyn CheckpointSaver>> {
    Err(GraphEngineError::Checkpointer(
        "phase graph Redis checkpointer requested, but sentinel-graph was built without the redis feature".into(),
    ))
}

fn validation_failed(message: impl Into<String>) -> StateError {
    StateError::ValidationFailed(message.into())
}

fn step_status_name(status: &StepStatus) -> &'static str {
    match status {
        StepStatus::Pending => "pending",
        StepStatus::InProgress => "in_progress",
        StepStatus::Completed => "completed",
        StepStatus::Skipped => "skipped",
        StepStatus::Blocked => "blocked",
    }
}

fn upsert_step_policy_evidence(
    state: &mut PhaseGraphState,
    phase_id: &str,
    step_policy: &WorkflowStep,
) {
    let evidence = StepRuntimePolicyEvidence::from_workflow_step(phase_id, step_policy);
    if let Some(existing) = state
        .step_policy_evidence
        .iter_mut()
        .find(|candidate| candidate.phase_id == phase_id && candidate.step_id == step_policy.id)
    {
        *existing = evidence;
    } else {
        state.step_policy_evidence.push(evidence);
    }
}

fn phase_graph_state_schema(
    workflow: &SkillWorkflow,
    phase_ids: &[String],
) -> StateSchema<PhaseGraphState> {
    let expected_skill = workflow.skill.clone();
    let expected_phase_order = phase_ids.to_vec();
    let required_phase_ids: Vec<String> = workflow
        .phases
        .iter()
        .filter(|phase| phase.required)
        .map(|phase| phase.id.clone())
        .collect();

    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(phase_graph_state_json_schema(
            workflow,
            phase_ids,
            &required_phase_ids,
        ))
        .with_validator(move |state| {
            validate_phase_graph_state(
                state,
                &expected_skill,
                &expected_phase_order,
                &required_phase_ids,
            )
        })
}

fn phase_graph_state_json_schema(
    workflow: &SkillWorkflow,
    phase_ids: &[String],
    required_phase_ids: &[String],
) -> serde_json::Value {
    let step_state_schema = phase_graph_step_state_json_schema(phase_ids);
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "skill",
            "session_id",
            "phase_order",
            "current_phase",
            "completed_phases",
            "complete",
            "dyad_verdicts",
            "step_states",
            "current_step",
            "last_verdict"
        ],
        "properties": {
            "skill": { "type": "string", "const": workflow.skill },
            "session_id": { "type": "string", "minLength": 1 },
            "phase_order": {
                "type": "array",
                "const": phase_ids,
                "items": { "type": "string" }
            },
            "current_phase": {
                "anyOf": [
                    { "type": "null" },
                    { "type": "integer", "minimum": 0, "maximum": phase_ids.len() }
                ]
            },
            "completed_phases": {
                "type": "array",
                "uniqueItems": true,
                "items": {
                    "type": "string",
                    "enum": phase_ids
                }
            },
            "complete": { "type": "boolean" },
            "dyad_verdicts": {
                "type": "object",
                "propertyNames": { "enum": phase_ids },
                "additionalProperties": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "implementer": { "type": "string" },
                        "reviewer_pass_by": { "type": "string" },
                        "tester_pass_by": { "type": "string" }
                    }
                }
            },
            "step_states": {
                "type": "array",
                "items": step_state_schema
            },
            "step_policy_evidence": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": [
                        "phase_id",
                        "step_id",
                        "retry_max_attempts",
                        "retry_backoff_ms",
                        "retry_on",
                        "circuit_failure_threshold",
                        "circuit_cooldown_ms"
                    ],
                    "properties": {
                        "phase_id": { "type": "string", "enum": phase_ids },
                        "step_id": { "type": "string", "minLength": 1 },
                        "timeout_ms": { "type": "integer" },
                        "retry_max_attempts": { "type": "integer", "minimum": 1 },
                        "retry_backoff_ms": { "type": "integer" },
                        "retry_on": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "circuit_failure_threshold": { "type": "integer" },
                        "circuit_cooldown_ms": { "type": "integer" }
                    }
                }
            },
            "current_step": {
                "anyOf": [
                    { "type": "null" },
                    { "type": "string", "minLength": 1 }
                ]
            },
            "replay_events": {
                "type": "array"
            },
            "phase_error_events": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["phase_id", "node_id", "error"],
                    "properties": {
                        "phase_id": { "type": "string", "enum": phase_ids },
                        "node_id": { "type": "string", "minLength": 1 },
                        "error": { "type": "string", "minLength": 1 }
                    }
                }
            },
            "last_verdict": {
                "type": "string",
                "enum": ["pending", "pass", "fail"]
            }
        },
        "x-sentinel": {
            "graph": PHASE_GRAPH_NAME,
            "workflow_skill": workflow.skill,
            "required_phases": required_phase_ids,
            "authority": "langgraph"
        }
    })
}

fn phase_graph_step_state_json_schema(phase_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["step_id", "phase_id", "status"],
        "properties": {
            "step_id": { "type": "string", "minLength": 1 },
            "phase_id": { "type": "string", "enum": phase_ids },
            "status": {
                "type": "string",
                "enum": ["pending", "in_progress", "completed", "skipped", "blocked"]
            },
            "started_at": { "type": "string", "format": "date-time" },
            "completed_at": { "type": "string", "format": "date-time" },
            "summary": { "type": "string" }
        }
    })
}

fn validate_phase_graph_state(
    state: &PhaseGraphState,
    expected_skill: &str,
    expected_phase_order: &[String],
    required_phase_ids: &[String],
) -> std::result::Result<(), StateError> {
    if state.skill != expected_skill {
        return Err(validation_failed(format!(
            "skill '{}' does not match compiled workflow '{}'",
            state.skill, expected_skill
        )));
    }
    if state.session_id.trim().is_empty() {
        return Err(validation_failed("session_id must not be empty"));
    }
    sentinel_domain::langgraph_thread::validate_thread_id_component(
        &state.session_id,
        "session_id",
    )
    .map_err(validation_failed)?;
    if state.phase_order != expected_phase_order {
        return Err(validation_failed(
            "phase_order must match the compiled workflow exactly",
        ));
    }

    let mut seen = BTreeSet::new();
    let mut previous_idx = None;
    for phase in &state.completed_phases {
        let Some(idx) = expected_phase_order
            .iter()
            .position(|candidate| candidate == phase)
        else {
            return Err(validation_failed(format!(
                "completed phase '{phase}' is not in the compiled workflow"
            )));
        };
        if !seen.insert(phase.as_str()) {
            return Err(validation_failed(format!(
                "completed phase '{phase}' appears more than once"
            )));
        }
        if previous_idx.is_some_and(|prev| idx <= prev) {
            return Err(validation_failed(
                "completed_phases must follow compiled phase order",
            ));
        }
        previous_idx = Some(idx);
    }

    match state.current_phase {
        Some(idx) if idx > expected_phase_order.len() => {
            return Err(validation_failed(format!(
                "current_phase index {idx} exceeds compiled phase count {}",
                expected_phase_order.len()
            )));
        }
        Some(idx) if idx == expected_phase_order.len() && !state.complete => {
            return Err(validation_failed(
                "current_phase can point past the last phase only when complete is true",
            ));
        }
        None if state.complete => {
            return Err(validation_failed(
                "complete graph state must have current_phase set to the terminal index",
            ));
        }
        None if !state.completed_phases.is_empty() => {
            return Err(validation_failed(
                "graph state with completed phases must have current_phase set",
            ));
        }
        _ => {}
    }

    let all_required_done = required_phase_ids
        .iter()
        .all(|required| state.completed_phases.iter().any(|done| done == required));
    if state.complete {
        if let Some(missing) = required_phase_ids
            .iter()
            .find(|required| !state.completed_phases.iter().any(|done| done == *required))
        {
            return Err(validation_failed(format!(
                "complete graph state is missing required phase '{missing}'"
            )));
        }
        if state.current_phase != Some(expected_phase_order.len()) {
            return Err(validation_failed(
                "complete graph state must use the terminal current_phase index",
            ));
        }
    } else if !required_phase_ids.is_empty() && all_required_done {
        return Err(validation_failed(
            "graph state with every required phase complete must set complete=true",
        ));
    }

    for phase in state.dyad_verdicts.keys() {
        if !expected_phase_order
            .iter()
            .any(|candidate| candidate == phase)
        {
            return Err(validation_failed(format!(
                "dyad verdict phase '{phase}' is not in the compiled workflow"
            )));
        }
    }

    for step in &state.step_states {
        if step.step_id.trim().is_empty() {
            return Err(validation_failed("step_state step_id must not be empty"));
        }
        if !expected_phase_order
            .iter()
            .any(|candidate| candidate == &step.phase_id)
        {
            return Err(validation_failed(format!(
                "step_state phase '{}' is not in the compiled workflow",
                step.phase_id
            )));
        }
    }
    let mut seen_step_policies = BTreeSet::new();
    for policy in &state.step_policy_evidence {
        if policy.step_id.trim().is_empty() {
            return Err(validation_failed(
                "step_policy_evidence step_id must not be empty",
            ));
        }
        if !expected_phase_order
            .iter()
            .any(|candidate| candidate == &policy.phase_id)
        {
            return Err(validation_failed(format!(
                "step_policy_evidence phase '{}' is not in the compiled workflow",
                policy.phase_id
            )));
        }
        if policy.retry_max_attempts == 0 {
            return Err(validation_failed(
                "step_policy_evidence retry_max_attempts must be at least 1",
            ));
        }
        if !seen_step_policies.insert((policy.phase_id.as_str(), policy.step_id.as_str())) {
            return Err(validation_failed(format!(
                "step_policy_evidence for '{}.{}' appears more than once",
                policy.phase_id, policy.step_id
            )));
        }
        if !state
            .step_states
            .iter()
            .any(|step| step.phase_id == policy.phase_id && step.step_id == policy.step_id)
        {
            return Err(validation_failed(format!(
                "step_policy_evidence '{}.{}' must reference a persisted step_state",
                policy.phase_id, policy.step_id
            )));
        }
    }
    for event in &state.replay_events {
        if !expected_phase_order
            .iter()
            .any(|candidate| candidate == &event.phase_id)
        {
            return Err(validation_failed(format!(
                "replay event phase '{}' is not in the compiled workflow",
                event.phase_id
            )));
        }
        if event.reason.trim().is_empty() {
            return Err(validation_failed("replay event reason must not be empty"));
        }
        for phase in &event.superseded_completed_phases {
            if !expected_phase_order
                .iter()
                .any(|candidate| candidate == phase)
            {
                return Err(validation_failed(format!(
                    "replay event superseded phase '{phase}' is not in the compiled workflow"
                )));
            }
        }
        for step in &event.superseded_step_states {
            if step.step_id.trim().is_empty() {
                return Err(validation_failed(
                    "replay event superseded step_id must not be empty",
                ));
            }
            if !matches!(
                step.status.as_str(),
                "pending" | "in_progress" | "completed" | "skipped" | "blocked"
            ) {
                return Err(validation_failed(format!(
                    "replay event superseded step status '{}' is invalid",
                    step.status
                )));
            }
            if !expected_phase_order
                .iter()
                .any(|candidate| candidate == &step.phase_id)
            {
                return Err(validation_failed(format!(
                    "replay event superseded step phase '{}' is not in the compiled workflow",
                    step.phase_id
                )));
            }
        }
    }
    for event in &state.phase_error_events {
        if !expected_phase_order
            .iter()
            .any(|candidate| candidate == &event.phase_id)
        {
            return Err(validation_failed(format!(
                "phase error event phase '{}' is not in the compiled workflow",
                event.phase_id
            )));
        }
        if event.node_id.trim().is_empty() {
            return Err(validation_failed(
                "phase error event node_id must not be empty",
            ));
        }
        if event.error.trim().is_empty() {
            return Err(validation_failed(
                "phase error event error must not be empty",
            ));
        }
    }
    if let Some(current_step) = &state.current_step {
        if current_step.trim().is_empty() {
            return Err(validation_failed("current_step must not be empty"));
        }
        if !state
            .step_states
            .iter()
            .any(|step| &step.step_id == current_step)
        {
            return Err(validation_failed(format!(
                "current_step '{current_step}' must reference a persisted step_state"
            )));
        }
    }

    Ok(())
}

/// Compile a [`SkillWorkflow`] into a checkpointed phase graph using a typed
/// checkpointer whose backend identity is preserved in topology introspection.
///
/// Topology: `START -> phase[0] -> phase[1] -> ... -> phase[n-1] -> END`,
/// with a conditional edge on every phase that routes Pass to the next phase
/// (or `END` for the last) and Fail back to the same phase. Each phase node
/// interrupts *after* execution so the out-of-process judge can supply the
/// verdict via resume.
///
/// # Errors
/// Returns [`GraphEngineError`] if the workflow has no phases or the graph
/// fails structural validation.
pub fn compile_skill_graph_with_checkpointer(
    workflow: &SkillWorkflow,
    checkpointer: PhaseCheckpointer,
) -> Result<CompiledPhaseGraph> {
    let backend = checkpointer.backend().to_string();
    let scope = checkpointer.scope().to_string();
    let tenant_scope = checkpointer.tenant_scope().map(ToOwned::to_owned);
    compile_skill_graph_with_backend(
        workflow,
        checkpointer.into_saver(),
        backend,
        scope,
        tenant_scope,
    )
}

fn compile_skill_graph_with_backend(
    workflow: &SkillWorkflow,
    checkpointer: Arc<dyn CheckpointSaver>,
    checkpointer_backend: String,
    checkpointer_scope: String,
    tenant_scope: Option<String>,
) -> Result<CompiledPhaseGraph> {
    if workflow.phases.is_empty() {
        return Err(GraphEngineError::EmptyWorkflow(workflow.skill.clone()));
    }
    validate_phase_checkpointer_tenant_scope(&checkpointer_backend, tenant_scope.as_deref())?;

    let phase_ids: Vec<String> = workflow.phases.iter().map(|p| p.id.clone()).collect();
    let schema = phase_graph_state_schema(workflow, &phase_ids);
    let checkpointer_tenant_scope = tenant_scope.as_deref().unwrap_or("").to_string();
    let mut builder = StateGraphBuilder::<PhaseGraphState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema);

    // One async node per phase. The node marks the phase active (records its index)
    // and is the interrupt point — actual phase *work* happens in the agent,
    // out of process; the node only advances bookkeeping. Using async nodes
    // keeps the phase engine on LangGraph's native async execution path instead
    // of blocking inside sync closures.
    for (idx, phase_id) in phase_ids.iter().enumerate() {
        let mut node_config = NodeConfig::new()
            .with_metadata("sentinel.graph", PHASE_GRAPH_NAME)
            .with_metadata("sentinel.node", phase_id.clone())
            .with_metadata("sentinel.skill", workflow.skill.clone())
            .with_metadata("sentinel.phase", phase_id.clone())
            .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)));
        node_config = node_config
            .with_metadata(CHECKPOINTER_BACKEND_METADATA, checkpointer_backend.clone())
            .with_metadata(CHECKPOINTER_SCOPE_METADATA, checkpointer_scope.clone())
            .with_metadata(
                CHECKPOINTER_TENANT_SCOPE_METADATA,
                checkpointer_tenant_scope.clone(),
            );
        let phase_id_for_event = phase_id.clone();
        let phase_id_for_handler = phase_id.clone();
        builder = builder.add_async_node_with_config_and_error_handler(
            phase_id.as_str(),
            move |state: PhaseGraphState| {
                let phase_id_for_event = phase_id_for_event.clone();
                async move {
                    let mut next = state.clone();
                    next.current_phase = Some(idx);
                    // The node itself does not decide Pass/Fail; the verdict arrives
                    // via resume. Reset to Pending so a stale verdict can't leak across
                    // phases.
                    next.last_verdict = Verdict::Pending;
                    get_stream_writer()
                        .ok_or_else(|| {
                            NodeError::ExecutionFailed(
                                "LangGraph custom stream writer missing for phase node".to_string(),
                            )
                        })?
                        .write(serde_json::json!({
                            "type": "sentinel.phase_gate",
                            "skill": &next.skill,
                            "session_id": &next.session_id,
                            "phase_id": &phase_id_for_event,
                            "phase_index": idx,
                            "last_verdict": next.last_verdict,
                        }))
                        .map_err(|err| NodeError::ExecutionFailed(err.to_string()))?;
                    Ok::<_, NodeError>(next)
                }
            },
            node_config,
            move |state: &PhaseGraphState, ctx: NodeErrorContext| {
                let mut next = state.clone();
                next.current_phase = Some(idx);
                next.complete = false;
                next.last_verdict = Verdict::Fail;
                next.phase_error_events.push(PhaseGraphErrorEvent {
                    phase_id: phase_id_for_handler.clone(),
                    node_id: ctx.node().to_string(),
                    error: ctx.error().to_string(),
                });
                Command::update(next)
            },
        );
    }

    // Re-enter from persisted graph state. A verdict checkpoint carries
    // `current_phase`; START routes to that phase instead of always replaying
    // phase zero.
    let start_order = phase_ids.clone();
    builder = builder.add_conditional_edge(START, move |state: &PhaseGraphState| {
        if state.complete {
            END.into()
        } else {
            state
                .current_phase
                .and_then(|idx| start_order.get(idx).cloned())
                .unwrap_or_else(|| start_order[0].clone())
                .into()
        }
    });

    // Conditional routing per phase: delegate to the pure `next_phase_target`
    // so the decision is unit-testable without driving the executor.
    for (idx, phase_id) in phase_ids.iter().enumerate() {
        let order = phase_ids.clone();
        let route_workflow = workflow.clone();
        builder =
            builder.add_conditional_edge(phase_id.as_str(), move |state: &PhaseGraphState| {
                next_required_phase_target(
                    state.last_verdict,
                    idx,
                    &order,
                    &route_workflow,
                    &state.completed_phases,
                )
                .into()
            });
    }

    let state_graph = builder.build().map_err(GraphEngineError::from_graph)?;

    // Compile with the checkpointer so execution state is durable across the
    // process-per-invocation hook model.
    let mut compilation = GraphCompiler::new()
        .with_checkpointer(checkpointer)
        .compile_with_config(state_graph)
        .map_err(GraphEngineError::from_graph)?;

    // Interrupt *after* every phase node: the graph runs the phase's
    // bookkeeping node, then pauses so the out-of-process judge can be applied
    // as a durable verdict checkpoint. `with_interrupt` is a builder on the
    // compiled graph, so re-fold it through the publicly-exposed `graph` field.
    let mut compiled = compilation.graph;
    for phase_id in &phase_ids {
        compiled = compiled.with_interrupt(phase_id.as_str(), InterruptConfig::after());
    }
    compilation.graph = compiled;

    Ok(CompiledPhaseGraph {
        inner: compilation,
        phase_ids,
        workflow: workflow.clone(),
        checkpointer_backend,
        checkpointer_scope,
        tenant_scope,
    })
}

/// Pure routing decision for the conditional edge leaving the phase at
/// `phase_idx`, given the judge `verdict` and the workflow's `phase_order`.
///
/// - `Pass` → the next phase id, or [`END`] if this was the last phase.
/// - `Fail` / `Pending` → the same phase id (loop back / stay paused).
///
/// This is the single source of truth for phase routing: the compiled graph's
/// conditional edges delegate here, and the graph authority uses it to route
/// after a durable verdict checkpoint.
#[must_use]
pub fn next_phase_target(verdict: Verdict, phase_idx: usize, phase_order: &[String]) -> String {
    let same = phase_order
        .get(phase_idx)
        .cloned()
        .unwrap_or_else(|| END.to_string());
    match verdict {
        Verdict::Pass => phase_order
            .get(phase_idx + 1)
            .cloned()
            .unwrap_or_else(|| END.to_string()),
        Verdict::Fail | Verdict::Pending => same,
    }
}

fn next_required_phase_target(
    verdict: Verdict,
    phase_idx: usize,
    phase_order: &[String],
    workflow: &SkillWorkflow,
    completed_phases: &[String],
) -> String {
    match verdict {
        Verdict::Pass => workflow
            .phases
            .iter()
            .find(|phase| phase.required && !completed_phases.contains(&phase.id))
            .map_or_else(|| END.to_string(), |phase| phase.id.clone()),
        Verdict::Fail | Verdict::Pending => phase_order
            .get(phase_idx)
            .cloned()
            .unwrap_or_else(|| END.to_string()),
    }
}

fn required_phase_node_metadata(
    skill: &str,
    nodes: &[PhaseGraphNodeInfo],
    metadata_key: &str,
    label: &str,
    expected: &str,
) -> Result<()> {
    if nodes.is_empty() {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' has no nodes"
        )));
    }
    for node in nodes {
        require_phase_node_metadata_value(skill, node, metadata_key, label, expected)?;
    }
    Ok(())
}

fn require_phase_node_metadata_value(
    skill: &str,
    node: &PhaseGraphNodeInfo,
    metadata_key: &str,
    label: &str,
    expected: &str,
) -> Result<()> {
    let Some(value) = node.metadata.get(metadata_key) else {
        return Err(GraphEngineError::Topology(format!(
            "phase graph node '{}' for skill '{skill}' is missing {metadata_key} metadata",
            node.id
        )));
    };
    if value != expected {
        return Err(GraphEngineError::Topology(format!(
            "phase graph node '{}' for skill '{skill}' has {label} '{value}', expected '{expected}'",
            node.id
        )));
    }
    Ok(())
}

fn required_phase_node_runtime_contract(
    skill: &str,
    phase_ids: &[String],
    nodes: &[PhaseGraphNodeInfo],
    checkpointer_backend: &str,
    checkpointer_scope: &str,
    checkpointer_tenant_scope: Option<&str>,
) -> Result<()> {
    validate_phase_checkpointer_tenant_scope(checkpointer_backend, checkpointer_tenant_scope)?;
    required_phase_node_metadata(
        skill,
        nodes,
        CHECKPOINTER_BACKEND_METADATA,
        "checkpointer backend",
        checkpointer_backend,
    )?;
    required_phase_node_metadata(
        skill,
        nodes,
        CHECKPOINTER_SCOPE_METADATA,
        "checkpointer scope",
        checkpointer_scope,
    )?;
    required_phase_node_metadata(
        skill,
        nodes,
        CHECKPOINTER_TENANT_SCOPE_METADATA,
        "checkpointer tenant scope",
        checkpointer_tenant_scope.unwrap_or(""),
    )?;

    let expected_nodes: BTreeSet<&str> = phase_ids.iter().map(String::as_str).collect();
    let actual_nodes: BTreeSet<&str> = nodes.iter().map(|node| node.id.as_str()).collect();
    for expected in expected_nodes.difference(&actual_nodes) {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' is missing phase node '{expected}'"
        )));
    }

    for node in nodes {
        if !expected_nodes.contains(node.id.as_str()) {
            return Err(GraphEngineError::Topology(format!(
                "phase graph for skill '{skill}' has unexpected LangGraph node '{}'",
                node.id
            )));
        }
        require_phase_node_metadata_value(
            skill,
            node,
            "sentinel.graph",
            "graph",
            PHASE_GRAPH_NAME,
        )?;
        require_phase_node_metadata_value(skill, node, "sentinel.node", "node", &node.id)?;
        require_phase_node_metadata_value(skill, node, "sentinel.skill", "skill", skill)?;
        require_phase_node_metadata_value(skill, node, "sentinel.phase", "phase", &node.id)?;
        if !node.has_error_handler {
            return Err(GraphEngineError::Topology(format!(
                "phase graph node '{}' for skill '{skill}' is missing a LangGraph error handler",
                node.id
            )));
        }
        if !node.has_timeout_policy {
            return Err(GraphEngineError::Topology(format!(
                "phase graph node '{}' for skill '{skill}' is missing a LangGraph timeout policy",
                node.id
            )));
        }
        if node.interrupt_before {
            return Err(GraphEngineError::Topology(format!(
                "phase graph node '{}' for skill '{skill}' must not interrupt before execution",
                node.id
            )));
        }
        if !node.interrupt_after {
            return Err(GraphEngineError::Topology(format!(
                "phase graph node '{}' for skill '{skill}' must interrupt after execution",
                node.id
            )));
        }
    }
    Ok(())
}

fn require_phase_schema_x_sentinel_value(
    skill: &str,
    state_schema: &serde_json::Value,
    key: &str,
    expected: &str,
) -> Result<()> {
    let Some(value) = state_schema
        .pointer(&format!("/x-sentinel/{key}"))
        .and_then(serde_json::Value::as_str)
    else {
        return Err(GraphEngineError::Topology(format!(
            "phase graph state schema for skill '{skill}' is missing x-sentinel.{key}"
        )));
    };
    if value != expected {
        return Err(GraphEngineError::Topology(format!(
            "phase graph state schema for skill '{skill}' has x-sentinel.{key} '{value}', expected '{expected}'"
        )));
    }
    Ok(())
}

fn required_phase_schema_contract(
    skill: &str,
    state: &Option<serde_json::Value>,
    input: &Option<serde_json::Value>,
    output: &Option<serde_json::Value>,
) -> Result<()> {
    let state_schema = state.as_ref().ok_or_else(|| {
        GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' is missing a state schema"
        ))
    })?;
    if input.is_none() {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' is missing an input schema"
        )));
    }
    if output.is_none() {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' is missing an output schema"
        )));
    }
    require_phase_schema_x_sentinel_value(skill, state_schema, "graph", PHASE_GRAPH_NAME)?;
    require_phase_schema_x_sentinel_value(skill, state_schema, "workflow_skill", skill)?;
    require_phase_schema_x_sentinel_value(skill, state_schema, "authority", "langgraph")?;
    Ok(())
}

fn required_phase_edge_contract(
    skill: &str,
    phase_ids: &[String],
    edges: &[PhaseGraphEdgeInfo],
) -> Result<()> {
    if edges.is_empty() {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' has no LangGraph edges"
        )));
    }

    let phase_nodes: BTreeSet<&str> = phase_ids.iter().map(String::as_str).collect();
    let mut required_sources: BTreeSet<&str> = phase_nodes.clone();
    required_sources.insert(START);

    for source in &required_sources {
        let matching: Vec<_> = edges
            .iter()
            .filter(|edge| edge.from == *source && edge.kind == "conditional")
            .collect();
        if matching.is_empty() {
            return Err(GraphEngineError::Topology(format!(
                "phase graph for skill '{skill}' is missing conditional routing from '{source}'"
            )));
        }
        if matching.len() > 1 {
            return Err(GraphEngineError::Topology(format!(
                "phase graph for skill '{skill}' has duplicate conditional routing from '{source}'"
            )));
        }
    }

    for edge in edges {
        if !required_sources.contains(edge.from.as_str()) {
            return Err(GraphEngineError::Topology(format!(
                "phase graph for skill '{skill}' has unexpected LangGraph edge source '{}'",
                edge.from
            )));
        }
        if edge.kind != "conditional" {
            return Err(GraphEngineError::Topology(format!(
                "phase graph for skill '{skill}' has non-conditional edge from '{}' with kind '{}'",
                edge.from, edge.kind
            )));
        }
    }

    Ok(())
}

fn required_phase_runtime_contract(
    skill: &str,
    durable_checkpointer: bool,
    auto_checkpoint: bool,
    max_iterations: usize,
    phase_count: usize,
) -> Result<()> {
    if !durable_checkpointer {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' is missing a durable LangGraph checkpointer"
        )));
    }
    if !auto_checkpoint {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' disabled LangGraph auto-checkpointing"
        )));
    }
    if max_iterations <= phase_count {
        return Err(GraphEngineError::Topology(format!(
            "phase graph for skill '{skill}' has max_iterations {max_iterations}, expected more than {phase_count} phases"
        )));
    }
    Ok(())
}

/// A compiled, checkpointed phase graph plus the ordered phase ids it was
/// built from. Wraps `langgraph-core`'s `CompilationResult` (which carries the
/// checkpointer for durable execution + time-travel).
pub struct CompiledPhaseGraph {
    inner: CompilationResult<PhaseGraphState>,
    phase_ids: Vec<String>,
    workflow: SkillWorkflow,
    checkpointer_backend: String,
    checkpointer_scope: String,
    tenant_scope: Option<String>,
}

impl CompiledPhaseGraph {
    fn validate_requested_skill(&self, skill: &str) -> Result<()> {
        if skill == self.workflow.skill {
            return Ok(());
        }
        Err(GraphEngineError::Graph(format!(
            "phase graph compiled for skill '{}' cannot be used as skill '{skill}'",
            self.workflow.skill
        )))
    }

    fn required_phase_ids(&self) -> Vec<String> {
        self.workflow
            .phases
            .iter()
            .filter(|phase| phase.required)
            .map(|phase| phase.id.clone())
            .collect()
    }

    fn validate_phase_state_for_session(
        &self,
        session_id: &str,
        state: &PhaseGraphState,
    ) -> Result<()> {
        if state.session_id != session_id {
            return Err(GraphEngineError::Graph(format!(
                "phase graph state session mismatch for skill '{}': expected '{session_id}', got '{}'",
                self.workflow.skill, state.session_id
            )));
        }
        validate_phase_graph_state(
            state,
            &self.workflow.skill,
            &self.phase_ids,
            &self.required_phase_ids(),
        )
        .map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase graph checkpoint state invalid for skill '{}' session '{session_id}': {err}",
                self.workflow.skill
            ))
        })
    }

    fn thread_id(&self, session_id: &str) -> Result<String> {
        sentinel_domain::langgraph_thread::phase_thread_id(
            &self.workflow.skill,
            session_id,
            self.tenant_scope.as_deref(),
        )
        .map_err(GraphEngineError::Checkpointer)
    }

    /// Derive the durable thread id from this compiled graph's captured
    /// checkpointer backend and tenant scope.
    ///
    /// This is the runtime-safe variant of [`phase_thread_id`]: it does not
    /// re-read process environment after the graph has been compiled.
    pub fn thread_id_for_session(&self, session_id: &str) -> Result<String> {
        self.thread_id(session_id)
    }

    /// The ordered phase ids of this workflow.
    #[must_use]
    pub fn phase_ids(&self) -> &[String] {
        &self.phase_ids
    }

    /// Reflect the compiled LangGraph topology for operator/API visibility.
    #[must_use]
    pub fn introspect(&self, session_id: &str) -> Result<PhaseGraphIntrospection> {
        let graph = &self.inner.graph;
        let nodes: Vec<_> = graph
            .node_ids()
            .filter_map(|node_id| {
                let node = graph.node_introspection(node_id.as_str())?;
                let interrupt = graph.interrupt_config(node.id);
                Some(PhaseGraphNodeInfo {
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
        required_phase_node_runtime_contract(
            &self.workflow.skill,
            &self.phase_ids,
            &nodes,
            &self.checkpointer_backend,
            &self.checkpointer_scope,
            self.tenant_scope.as_deref(),
        )?;

        let edges: Vec<PhaseGraphEdgeInfo> = graph
            .edge_descriptors()
            .map(|edge| PhaseGraphEdgeInfo {
                from: edge.from.to_string(),
                kind: format!("{:?}", edge.kind).to_ascii_lowercase(),
                to: edge.to.map(ToOwned::to_owned),
            })
            .collect();
        required_phase_edge_contract(&self.workflow.skill, &self.phase_ids, &edges)?;
        let schemas = graph.schemas_json();
        required_phase_schema_contract(
            &self.workflow.skill,
            &schemas.state,
            &schemas.input,
            &schemas.output,
        )?;
        let durable_checkpointer = self.inner.checkpointer.is_some();
        let auto_checkpoint = graph.auto_checkpoint();
        let max_iterations = graph.max_iterations();
        required_phase_runtime_contract(
            &self.workflow.skill,
            durable_checkpointer,
            auto_checkpoint,
            max_iterations,
            self.phase_ids.len(),
        )?;

        Ok(PhaseGraphIntrospection {
            skill: self.workflow.skill.clone(),
            thread_id: self.thread_id(session_id)?,
            phase_order: self.phase_ids.clone(),
            durable_checkpointer,
            checkpointer_backend: self.checkpointer_backend.clone(),
            checkpointer_scope: self.checkpointer_scope.clone(),
            checkpointer_tenant_scope: self.tenant_scope.clone(),
            auto_checkpoint,
            max_iterations,
            schemas: PhaseGraphSchemas {
                state: schemas.state,
                input: schemas.input,
                output: schemas.output,
                context: schemas.context,
            },
            nodes,
            edges,
        })
    }

    /// Load the latest checkpointed state for a session, if any.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] on checkpointer failure.
    pub async fn load_latest(&self, session_id: &str) -> Result<Option<PhaseGraphState>> {
        let thread_id = self.thread_id(session_id)?;
        let state = self
            .inner
            .get_state(&thread_id)
            .await
            .map_err(GraphEngineError::from_graph)?;
        if let Some(state) = &state {
            self.validate_latest_checkpoint_state(session_id, state)
                .await?;
        }
        Ok(state)
    }

    /// Load the checkpointed state for a session, or initialise a fresh one
    /// (at `current_phase = Some(0)`) when no checkpoint exists yet.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] on checkpointer failure.
    pub async fn load_or_init(&self, skill: &str, session_id: &str) -> Result<PhaseGraphState> {
        self.validate_requested_skill(skill)?;
        if let Some(existing) = self.load_latest(session_id).await? {
            return Ok(existing);
        }
        let mut fresh = PhaseGraphState::new(skill, session_id, self.phase_ids.clone());
        fresh.current_phase = Some(0);
        Ok(fresh)
    }

    /// Execute the compiled graph until it reaches the next judge interrupt.
    ///
    /// This is Sentinel's normal "phase gate" execution boundary: LangGraph
    /// runs the current phase bookkeeping node, persists a checkpoint under the
    /// caller's `thread_id`, then raises an interrupt. The interrupt is expected
    /// control flow, so this method loads and returns the checkpointed state.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] if graph execution fails for any reason
    /// other than an intentional interrupt, or if no checkpoint was written.
    pub async fn run_until_gate(&self, skill: &str, session_id: &str) -> Result<PhaseGraphState> {
        Ok(self.run_until_gate_report(skill, session_id).await?.state)
    }

    /// Execute the compiled graph until the next judge interrupt and return
    /// both the checkpointed phase state and the LangGraph stream parts emitted
    /// by that run.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] if the stream closes without the expected
    /// phase-node checkpoint and matching active interrupt, or if a node error
    /// is observed in the stream.
    pub async fn run_until_gate_report(
        &self,
        skill: &str,
        session_id: &str,
    ) -> Result<PhaseGraphRunReport> {
        let state = self.load_or_init(skill, session_id).await?;
        self.execute_until_gate_report(session_id, state).await
    }

    async fn execute_until_gate_report(
        &self,
        session_id: &str,
        input: PhaseGraphState,
    ) -> Result<PhaseGraphRunReport> {
        self.validate_phase_state_for_session(session_id, &input)?;
        let expected_gate = input
            .current_phase
            .and_then(|idx| self.phase_ids.get(idx).cloned())
            .ok_or_else(|| {
                GraphEngineError::Graph(format!(
                    "phase stream requires an active phase for session {session_id}"
                ))
            })?;
        let expected_skill = input.skill.clone();
        let thread_id = self.thread_id(session_id)?;
        let config = serde_json::json!({
            "configurable": {
                "thread_id": thread_id,
            }
        });

        let checkpointer = self.inner.checkpointer.as_ref().ok_or_else(|| {
            GraphEngineError::Graph(
                "phase stream requires a configured durable LangGraph checkpointer".into(),
            )
        })?;
        let handle = self
            .inner
            .graph
            .stream_events_v3_with_options(
                input,
                Self::phase_v3_transformers(),
                RunV3Options::new()
                    .with_checkpointer(Arc::clone(checkpointer))
                    .with_thread_id(thread_id.clone())
                    .with_config(config),
            )
            .await
            .map_err(GraphEngineError::from_graph)?;
        let v3_run = Self::collect_v3_phase_stream(handle, &thread_id).await?;
        let stream = v3_run.stream;
        let mut saw_expected_gate_checkpoint = false;
        let mut saw_expected_custom_phase_gate = false;
        let mut checkpoint_sources = Vec::new();

        for part in &stream {
            if part.payload_kind == "checkpoints" {
                let source_type = part
                    .payload_json
                    .pointer("/source/type")
                    .and_then(serde_json::Value::as_str);
                let source_node = part
                    .payload_json
                    .pointer("/source/node")
                    .and_then(serde_json::Value::as_str);
                checkpoint_sources.push(format!("{source_type:?}:{source_node:?}"));
                if source_type == Some("stream_update")
                    && source_node == Some(expected_gate.as_str())
                {
                    saw_expected_gate_checkpoint = true;
                }
            }
            if part.payload_kind == "custom"
                && part
                    .payload_json
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    == Some("sentinel.phase_gate")
                && part
                    .payload_json
                    .get("skill")
                    .and_then(serde_json::Value::as_str)
                    == Some(expected_skill.as_str())
                && part
                    .payload_json
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(session_id)
                && part
                    .payload_json
                    .get("phase_id")
                    .and_then(serde_json::Value::as_str)
                    == Some(expected_gate.as_str())
            {
                saw_expected_custom_phase_gate = true;
            }
        }
        if stream.is_empty() {
            return Err(GraphEngineError::Graph(format!(
                "phase stream emitted no parts for thread {thread_id}"
            )));
        }
        if !stream
            .iter()
            .all(|part| part.stream_protocol == PHASE_STREAM_PROTOCOL)
        {
            return Err(GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} emitted non-v3 LangGraph stream evidence"
            )));
        }
        Self::require_phase_stream_payload_kind(&stream, &thread_id, "values", "values")?;
        Self::require_phase_stream_payload_kind(&stream, &thread_id, "updates", "v3 updates")?;
        Self::require_phase_stream_payload_kind(&stream, &thread_id, "tasks", "v3 task")?;
        Self::require_phase_stream_payload_kind(&stream, &thread_id, "debug", "v3 debug")?;
        if !saw_expected_gate_checkpoint {
            return Err(GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} did not persist expected gate checkpoint for node {expected_gate}; observed checkpoint sources: {}",
                checkpoint_sources.join(", ")
            )));
        }
        if !saw_expected_custom_phase_gate {
            return Err(GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} omitted Sentinel custom phase-gate payload for node {expected_gate}"
            )));
        }

        let state = self.load_latest(session_id).await?.ok_or_else(|| {
            GraphEngineError::Graph("phase stream did not persist a checkpoint".into())
        })?;
        let snapshots = self.phase_snapshots(session_id).await?;
        let latest = snapshots.last().ok_or_else(|| {
            GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} did not expose checkpoint history"
            ))
        })?;
        if v3_run.output != state {
            return Err(GraphEngineError::Graph(format!(
                "phase v3 stream output state mismatch for thread {thread_id}"
            )));
        }
        if latest.state != state {
            return Err(GraphEngineError::Graph(format!(
                "phase stream latest checkpoint state mismatch for thread {thread_id}"
            )));
        }
        Self::validate_stream_latest_checkpoint_evidence(
            &stream,
            latest,
            &thread_id,
            &expected_gate,
            &state,
        )?;
        if !v3_run.interrupted {
            return Err(GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} completed without the expected gate interrupt"
            )));
        }
        if !v3_run
            .interrupts
            .iter()
            .any(|interrupt| interrupt.node_id() == expected_gate)
        {
            return Err(GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} did not activate expected interrupt for node {expected_gate}"
            )));
        }

        Ok(PhaseGraphRunReport { state, stream })
    }

    fn require_phase_stream_payload_kind(
        stream: &[PhaseGraphStreamPart],
        thread_id: &str,
        payload_kind: &str,
        label: &str,
    ) -> Result<()> {
        if stream.iter().any(|part| part.payload_kind == payload_kind) {
            Ok(())
        } else {
            Err(GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} omitted LangGraph {label} payloads"
            )))
        }
    }

    fn phase_v3_transformers() -> Vec<Arc<dyn V3StreamTransformer>> {
        vec![
            Arc::new(UpdatesTransformer),
            Arc::new(CheckpointsTransformer),
            Arc::new(TasksTransformer),
            Arc::new(DebugTransformer),
            Arc::new(CustomTransformer),
        ]
    }

    async fn collect_v3_phase_stream(
        handle: GraphRunStream<PhaseGraphState>,
        thread_id: &str,
    ) -> Result<PhaseV3StreamRun> {
        let values_rx = handle
            .values()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let updates_rx = handle
            .updates()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let checkpoints_rx = handle
            .checkpoints()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let tasks_rx = handle
            .tasks()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let debug_rx = handle
            .debug()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let custom_rx = handle
            .custom()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let lifecycle_rx = handle
            .lifecycle()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let subgraphs_rx = handle
            .subgraphs()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
        let messages_rx = handle
            .messages()
            .take()
            .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
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
            Self::drain_v3_values(values_rx),
            Self::drain_v3_updates(updates_rx),
            Self::drain_v3_checkpoints(checkpoints_rx),
            Self::drain_v3_tasks(tasks_rx),
            Self::drain_v3_debug(debug_rx),
            Self::drain_v3_custom(custom_rx),
            Self::drain_v3_lifecycle(lifecycle_rx),
            Self::drain_v3_subgraphs(subgraphs_rx),
            Self::drain_v3_messages(messages_rx),
            output,
        );

        let mut drain = V3PhaseProjectionDrain::default();
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
            return Err(GraphEngineError::Graph(error));
        }

        let output = output.map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} closed before LangGraph v3 completion: {err}"
            ))
        })?;
        let interrupted = handle.interrupted().await.map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} did not expose v3 interrupted status: {err}"
            ))
        })?;
        let interrupts = handle.interrupts().await.map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase stream for thread {thread_id} did not expose v3 interrupts: {err}"
            ))
        })?;
        let superstep = drain
            .stream
            .iter()
            .map(|part| part.superstep)
            .max()
            .unwrap_or(0)
            + 1;
        drain.stream.push(Self::phase_stream_part(
            "ExecutionComplete",
            END,
            chrono::Utc::now().to_rfc3339(),
            superstep,
            "values",
            Self::phase_state_payload_json(&output),
            Vec::new(),
        ));
        drain.stream.sort_by(|left, right| {
            left.superstep
                .cmp(&right.superstep)
                .then_with(|| left.timestamp.cmp(&right.timestamp))
                .then_with(|| Self::stream_part_rank(left).cmp(&Self::stream_part_rank(right)))
                .then_with(|| left.node_id.cmp(&right.node_id))
                .then_with(|| left.payload_kind.cmp(&right.payload_kind))
        });

        Ok(PhaseV3StreamRun {
            output,
            interrupted,
            interrupts,
            stream: drain.stream,
        })
    }

    async fn drain_v3_values(
        mut rx: mpsc::Receiver<StateSnapshot<PhaseGraphState>>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(snapshot) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "Values",
                STATE_SNAPSHOT_NODE,
                chrono::Utc::now().to_rfc3339(),
                snapshot.superstep(),
                "values",
                Self::phase_state_payload_json(snapshot.state()),
                Vec::new(),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_updates(
        mut rx: mpsc::Receiver<StateUpdateEvent>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(event) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "Updates",
                event.node_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.superstep,
                "updates",
                event.payload,
                Self::namespace_strings(&event.subgraph_namespace),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_checkpoints(
        mut rx: mpsc::Receiver<CheckpointEvent>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(event) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "Checkpoint",
                event.node_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.superstep,
                "checkpoints",
                event.payload,
                Self::namespace_strings(&event.subgraph_namespace),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_tasks(mut rx: mpsc::Receiver<TaskEvent>) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(event) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "Task",
                event.node_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.superstep,
                "tasks",
                event.payload,
                Self::namespace_strings(&event.subgraph_namespace),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_debug(mut rx: mpsc::Receiver<DebugEvent>) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(event) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "Debug",
                event.node_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.superstep,
                "debug",
                serde_json::to_value(&event.info)
                    .map_err(|err| GraphEngineError::Graph(err.to_string()))?,
                Self::namespace_strings(&event.subgraph_namespace),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_custom(
        mut rx: mpsc::Receiver<CustomEvent>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(event) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "Custom",
                event.node_id.to_string(),
                event.timestamp.to_rfc3339(),
                event.superstep,
                "custom",
                event.payload,
                Self::namespace_strings(&event.subgraph_namespace),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_lifecycle(
        mut rx: mpsc::Receiver<LifecycleEvent>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(event) = rx.recv().await {
            let (event_type, node_id) = match &event {
                LifecycleEvent::SubgraphStarted { node_id, .. } => ("SubgraphStarted", node_id),
                LifecycleEvent::SubgraphCompleted { node_id, .. } => ("SubgraphCompleted", node_id),
                LifecycleEvent::SubgraphFailed { node_id, .. } => ("SubgraphFailed", node_id),
                _ => {
                    return Err(GraphEngineError::Graph(
                        "unsupported non-exhaustive LangGraph v3 lifecycle event in phase stream"
                            .into(),
                    ));
                }
            };
            drain.push(Self::phase_stream_part(
                event_type,
                node_id.to_string(),
                chrono::Utc::now().to_rfc3339(),
                0,
                "lifecycle",
                serde_json::to_value(&event)
                    .map_err(|err| GraphEngineError::Graph(err.to_string()))?,
                Self::namespace_strings(event.subgraph_namespace()),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_subgraphs(
        mut rx: mpsc::Receiver<SubgraphHandle>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(handle) = rx.recv().await {
            let full_path = handle
                .full_path()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            drain.push(Self::phase_stream_part(
                "Subgraph",
                handle.node_id().to_string(),
                chrono::Utc::now().to_rfc3339(),
                0,
                "subgraphs",
                serde_json::json!({
                    "node_id": handle.node_id().to_string(),
                    "full_path": full_path,
                }),
                Self::namespace_strings(handle.subgraph_namespace()),
            ));
        }
        Ok(drain)
    }

    async fn drain_v3_messages(
        mut rx: mpsc::Receiver<ChatModelStream>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(message) = rx.recv().await {
            let node_id = message.node_id.to_string();
            drain.push(Self::phase_stream_part(
                "MessageStreamStarted",
                node_id.clone(),
                chrono::Utc::now().to_rfc3339(),
                0,
                "messages",
                serde_json::json!({
                    "node_id": node_id.clone(),
                    "stream": "chat_model",
                }),
                Vec::new(),
            ));
            let text_rx = Projection::take(&message.text)
                .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
            let reasoning_rx = Projection::take(&message.reasoning)
                .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
            let tool_calls_rx = Projection::take(&message.tool_calls)
                .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
            let usage_rx = Projection::take(&message.usage)
                .map_err(|err| GraphEngineError::Graph(err.to_string()))?;
            let output = message.output();
            let (text, reasoning, tool_calls, usage, output) = tokio::join!(
                Self::drain_chat_message_text(node_id.clone(), text_rx),
                Self::drain_chat_message_reasoning(node_id.clone(), reasoning_rx),
                Self::drain_chat_message_tool_calls(node_id.clone(), tool_calls_rx),
                Self::drain_chat_message_usage(node_id.clone(), usage_rx),
                output,
            );
            drain.extend(text?);
            drain.extend(reasoning?);
            drain.extend(tool_calls?);
            drain.extend(usage?);

            let output = output.map_err(|err| {
                GraphEngineError::Graph(format!(
                    "phase graph message stream for node {node_id} closed without output: {err}"
                ))
            })?;
            drain.push(Self::phase_stream_part(
                "MessageOutput",
                node_id,
                chrono::Utc::now().to_rfc3339(),
                0,
                "messages",
                serde_json::json!({
                    "channel": "output",
                    "message": serde_json::to_value(&output)
                        .map_err(|err| GraphEngineError::Graph(err.to_string()))?,
                }),
                Vec::new(),
            ));
        }
        Ok(drain)
    }

    async fn drain_chat_message_text(
        node_id: String,
        mut rx: mpsc::Receiver<String>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(delta) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "MessageTextDelta",
                node_id.clone(),
                chrono::Utc::now().to_rfc3339(),
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
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(delta) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "MessageReasoningDelta",
                node_id.clone(),
                chrono::Utc::now().to_rfc3339(),
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
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(tool_call) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "MessageToolCall",
                node_id.clone(),
                chrono::Utc::now().to_rfc3339(),
                0,
                "messages",
                serde_json::json!({
                    "channel": "tool_calls",
                    "tool_call": serde_json::to_value(&tool_call)
                        .map_err(|err| GraphEngineError::Graph(err.to_string()))?,
                }),
                Vec::new(),
            ));
        }
        Ok(drain)
    }

    async fn drain_chat_message_usage(
        node_id: String,
        mut rx: mpsc::Receiver<langgraph_core::domain::value_objects::TokenUsage>,
    ) -> Result<V3PhaseProjectionDrain> {
        let mut drain = V3PhaseProjectionDrain::default();
        while let Some(usage) = rx.recv().await {
            drain.push(Self::phase_stream_part(
                "MessageUsage",
                node_id.clone(),
                chrono::Utc::now().to_rfc3339(),
                0,
                "messages",
                serde_json::json!({
                    "channel": "usage",
                    "usage": serde_json::to_value(&usage)
                        .map_err(|err| GraphEngineError::Graph(err.to_string()))?,
                }),
                Vec::new(),
            ));
        }
        Ok(drain)
    }

    fn phase_stream_part(
        event_type: impl Into<String>,
        node_id: impl Into<String>,
        timestamp: String,
        superstep: u64,
        payload_kind: impl Into<String>,
        payload_json: serde_json::Value,
        subgraph_namespace: Vec<String>,
    ) -> PhaseGraphStreamPart {
        PhaseGraphStreamPart {
            stream_protocol: PHASE_STREAM_PROTOCOL.to_string(),
            event_type: event_type.into(),
            node_id: node_id.into(),
            timestamp,
            superstep,
            payload_kind: payload_kind.into(),
            payload_json,
            subgraph_namespace,
        }
    }

    fn namespace_strings(
        namespace: &[langgraph_core::domain::value_objects::NodeId],
    ) -> Vec<String> {
        namespace.iter().map(ToString::to_string).collect()
    }

    fn stream_part_rank(part: &PhaseGraphStreamPart) -> u8 {
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

    fn phase_state_payload_json(state: &PhaseGraphState) -> serde_json::Value {
        let replay_events: Vec<_> = state
            .replay_events
            .iter()
            .map(|event| {
                let superseded_step_states: Vec<_> = event
                    .superseded_step_states
                    .iter()
                    .map(|step| {
                        serde_json::json!({
                            "step_id": &step.step_id,
                            "phase_id": &step.phase_id,
                            "status": &step.status,
                            "started_at": &step.started_at,
                            "completed_at": &step.completed_at,
                            "summary": &step.summary,
                        })
                    })
                    .collect();
                serde_json::json!({
                    "phase_id": &event.phase_id,
                    "reason": &event.reason,
                    "superseded_completed_phases": &event.superseded_completed_phases,
                    "superseded_step_states": superseded_step_states,
                })
            })
            .collect();
        serde_json::json!({
            "skill": &state.skill,
            "session_id": &state.session_id,
            "phase_order": &state.phase_order,
            "current_phase": state.current_phase,
            "completed_phases": &state.completed_phases,
            "complete": state.complete,
            "dyad_verdicts": &state.dyad_verdicts,
            "step_states": &state.step_states,
            "step_policy_evidence": &state.step_policy_evidence,
            "current_step": &state.current_step,
            "replay_events": replay_events,
            "phase_error_events": &state.phase_error_events,
            "last_verdict": state.last_verdict,
        })
    }

    pub(crate) fn validate_stream_latest_checkpoint_evidence(
        stream: &[PhaseGraphStreamPart],
        latest: &PhaseGraphCheckpointSnapshot,
        expected_thread_id: &str,
        expected_gate: &str,
        expected_state: &PhaseGraphState,
    ) -> Result<()> {
        if let Some(part) = stream
            .iter()
            .find(|part| part.stream_protocol != PHASE_STREAM_PROTOCOL)
        {
            return Err(GraphEngineError::Graph(format!(
                "phase stream for thread {expected_thread_id} contains {} evidence, expected {PHASE_STREAM_PROTOCOL}",
                part.stream_protocol
            )));
        }

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
                GraphEngineError::Graph(format!(
                    "phase stream for thread {expected_thread_id} omitted latest checkpoint payload for checkpoint '{}'",
                    latest.checkpoint_id
                ))
            })?;

        if checkpoint_part.event_type != "Checkpoint" {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' has event type '{}', expected 'Checkpoint'",
                latest.checkpoint_id, checkpoint_part.event_type
            )));
        }
        if checkpoint_part.node_id != expected_gate {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' node mismatch: got '{}', expected '{expected_gate}'",
                latest.checkpoint_id, checkpoint_part.node_id
            )));
        }

        let stream_thread = checkpoint_part
            .payload_json
            .get("thread_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                GraphEngineError::Graph(format!(
                    "phase stream checkpoint '{}' omitted thread_id",
                    latest.checkpoint_id
                ))
            })?;
        if stream_thread != expected_thread_id || latest.thread_id != expected_thread_id {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' thread mismatch: stream='{stream_thread}', latest='{}', expected '{expected_thread_id}'",
                latest.checkpoint_id, latest.thread_id
            )));
        }

        let stream_step = checkpoint_part
            .payload_json
            .get("step_number")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                GraphEngineError::Graph(format!(
                    "phase stream checkpoint '{}' omitted numeric step_number",
                    latest.checkpoint_id
                ))
            })?;
        if stream_step != latest.step_number {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' step mismatch: stream={stream_step}, latest={}",
                latest.checkpoint_id, latest.step_number
            )));
        }

        let stream_parent = match checkpoint_part.payload_json.get("parent_checkpoint_id") {
            Some(serde_json::Value::String(parent)) => Some(parent.as_str()),
            Some(serde_json::Value::Null) => None,
            Some(_) => {
                return Err(GraphEngineError::Graph(format!(
                    "phase stream checkpoint '{}' emitted non-string parent_checkpoint_id",
                    latest.checkpoint_id
                )));
            }
            None => {
                return Err(GraphEngineError::Graph(format!(
                    "phase stream checkpoint '{}' omitted parent_checkpoint_id",
                    latest.checkpoint_id
                )));
            }
        };
        if stream_parent != latest.parent_checkpoint_id.as_deref() {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' parent mismatch",
                latest.checkpoint_id
            )));
        }

        let source_type = checkpoint_part
            .payload_json
            .pointer("/source/type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                GraphEngineError::Graph(format!(
                    "phase stream checkpoint '{}' omitted source type",
                    latest.checkpoint_id
                ))
            })?;
        if source_type != "stream_update" {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' source type mismatch: got '{source_type}', expected 'stream_update'",
                latest.checkpoint_id
            )));
        }
        let source_node = checkpoint_part
            .payload_json
            .pointer("/source/node")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                GraphEngineError::Graph(format!(
                    "phase stream checkpoint '{}' omitted source node",
                    latest.checkpoint_id
                ))
            })?;
        if source_node != expected_gate {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' source node mismatch: got '{source_node}', expected '{expected_gate}'",
                latest.checkpoint_id
            )));
        }

        let stream_state = checkpoint_part.payload_json.get("state").ok_or_else(|| {
            GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' omitted state",
                latest.checkpoint_id
            ))
        })?;
        let expected_json = serde_json::to_value(expected_state).map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase stream failed to serialize expected checkpoint state: {err}"
            ))
        })?;
        if stream_state != &expected_json {
            return Err(GraphEngineError::Graph(format!(
                "phase stream checkpoint '{}' state mismatch",
                latest.checkpoint_id
            )));
        }

        Ok(())
    }

    fn complete_active_interrupts(&self) -> Result<()> {
        let mut interrupt_state = self
            .inner
            .graph
            .interrupt_state()
            .write()
            .map_err(|_| GraphEngineError::Graph("interrupt state lock poisoned".into()))?;
        let active_ids: Vec<_> = interrupt_state
            .active_interrupts()
            .into_iter()
            .map(|interrupt| interrupt.id().clone())
            .collect();
        for interrupt_id in active_ids {
            interrupt_state.complete_interrupt(&interrupt_id);
        }
        Ok(())
    }

    /// Apply a judge verdict for `phase_id` and persist the resulting state as
    /// a new checkpoint (parent-linked to the prior one, so the history forms
    /// the time-travel fork tree).
    ///
    /// `passed = true` advances to the next phase (or marks the workflow
    /// complete after the last phase); `passed = false` keeps `current_phase`
    /// on the failed phase (loop-back). This mirrors [`next_phase_target`] and
    /// produces the [`WorkflowState`] transition that Sentinel exposes to hooks
    /// and local clients, with this graph checkpoint as the durable authority.
    ///
    /// Returns the new state. The semantics of the out-of-process judge gate
    /// are realised here with a durable interrupt checkpoint: the phase node
    /// pauses, this call checkpoints the external verdict, then the graph is
    /// re-invoked on the same thread so LangGraph routing supplies the next
    /// state.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] if `phase_id` is unknown to this workflow,
    /// or on checkpointer failure.
    pub async fn apply_verdict(
        &self,
        skill: &str,
        session_id: &str,
        phase_id: &str,
        passed: bool,
    ) -> Result<PhaseGraphState> {
        Ok(self
            .apply_verdict_report(skill, session_id, phase_id, passed)
            .await?
            .state)
    }

    /// Apply a judge verdict and return the graph-owned state plus any
    /// LangGraph stream parts emitted while re-entering the next gate.
    ///
    /// Terminal-complete verdicts do not have a next gate to stream; in that
    /// case the returned stream is empty and the state is the terminal
    /// checkpointed graph state.
    pub async fn apply_verdict_report(
        &self,
        skill: &str,
        session_id: &str,
        phase_id: &str,
        passed: bool,
    ) -> Result<PhaseGraphRunReport> {
        self.validate_requested_skill(skill)?;
        let idx = self
            .phase_ids
            .iter()
            .position(|p| p == phase_id)
            .ok_or_else(|| GraphEngineError::UnknownPhase {
                skill: skill.to_string(),
                phase: phase_id.to_string(),
            })?;

        let mut state = self.load_latest(session_id).await?.ok_or_else(|| {
            GraphEngineError::MissingCheckpoint {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            }
        })?;
        if state.completed_phases.iter().any(|p| p == phase_id) {
            return Err(GraphEngineError::PhaseAlreadyCompleted {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            });
        }
        let verdict = if passed { Verdict::Pass } else { Verdict::Fail };
        state.last_verdict = verdict;
        state.current_phase = Some(idx);

        if passed {
            for prior in &self.workflow.phases[..idx] {
                if prior.required && !state.completed_phases.iter().any(|p| p == &prior.id) {
                    return Err(GraphEngineError::PhaseOrderViolation {
                        skill: skill.to_string(),
                        phase: phase_id.to_string(),
                        missing: prior.id.clone(),
                    });
                }
            }

            let phase = &self.workflow.phases[idx];
            if !state.to_workflow_state().dyad_satisfied(phase) {
                return Err(GraphEngineError::DyadUnsatisfied {
                    skill: skill.to_string(),
                    phase: phase_id.to_string(),
                });
            }

            state.completed_phases.push(phase_id.to_string());
            if self
                .workflow
                .phases
                .iter()
                .filter(|phase| phase.required)
                .all(|phase| state.completed_phases.iter().any(|p| p == &phase.id))
            {
                state.complete = true;
                state.current_phase = Some(self.phase_ids.len());
            } else if let Some(next_idx) = self
                .workflow
                .phases
                .iter()
                .position(|phase| phase.required && !state.completed_phases.contains(&phase.id))
            {
                state.current_phase = Some(next_idx);
            }
        } else {
            // Loop back: stay on this phase.
            state.current_phase = Some(idx);
            state.complete = false;
        }

        self.complete_active_interrupts()?;
        self.update_state_as_node(session_id, state.clone(), phase_id)
            .await?;

        if state.complete {
            Ok(PhaseGraphRunReport {
                state,
                stream: Vec::new(),
            })
        } else {
            self.execute_until_gate_report(session_id, state).await
        }
    }

    /// Persist a step-status transition through the durable LangGraph timeline.
    ///
    /// Step updates do not advance phase routing, so this stores the edited
    /// state as a checkpoint attributed to the owning phase node. A prior
    /// phase-gate checkpoint is required; step state cannot bootstrap a graph
    /// thread from session-local workflow data. The next graph invocation
    /// re-enters normal conditional routing from `START` using the checkpointed
    /// phase position.
    pub async fn update_step(
        &self,
        skill: &str,
        session_id: &str,
        phase_id: &str,
        step_id: &str,
        step_policy: &WorkflowStep,
        status: StepStatus,
        summary: Option<String>,
    ) -> Result<PhaseGraphState> {
        self.validate_requested_skill(skill)?;
        if !self.phase_ids.iter().any(|id| id == phase_id) {
            return Err(GraphEngineError::UnknownPhase {
                skill: skill.to_string(),
                phase: phase_id.to_string(),
            });
        }
        if step_policy.id != step_id {
            return Err(GraphEngineError::Graph(format!(
                "step policy id '{}' does not match requested step '{step_id}' in phase '{phase_id}' for skill '{skill}'",
                step_policy.id
            )));
        }

        let mut state = self.load_latest(session_id).await?.ok_or_else(|| {
            GraphEngineError::MissingCheckpoint {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            }
        })?;
        if let Some(existing) = state
            .step_states
            .iter()
            .find(|step| step.phase_id == phase_id && step.step_id == step_id)
        {
            if matches!(existing.status, StepStatus::Completed | StepStatus::Skipped) {
                return Err(GraphEngineError::StepAlreadyTerminal {
                    skill: skill.to_string(),
                    session_id: session_id.to_string(),
                    phase: phase_id.to_string(),
                    step_id: step_id.to_string(),
                    status: step_status_name(&existing.status).to_string(),
                });
            }
        }

        let mut workflow_state = state.to_workflow_state();
        workflow_state.update_step(phase_id, step_id, status, summary);
        state.step_states = workflow_state.step_states;
        upsert_step_policy_evidence(&mut state, phase_id, step_policy);
        state.current_step = workflow_state.current_step;

        self.complete_active_interrupts()?;
        self.update_state_as_node(session_id, state, phase_id).await
    }

    /// Persist role-dyad authorization context through the durable LangGraph
    /// timeline.
    ///
    /// Dyad verdicts authorize phase completion for phases that declare
    /// `required_dyad`; accepting them from a cached [`WorkflowState`] at phase
    /// completion time would recreate a non-graph authority path. Callers must
    /// write dyad context here first, then submit the phase verdict against the
    /// checkpointed graph state. A prior phase-gate checkpoint is required so
    /// dyad context cannot bootstrap graph authority from local cache.
    pub async fn update_dyad_verdicts(
        &self,
        skill: &str,
        session_id: &str,
        phase_id: &str,
        verdicts: DyadVerdicts,
    ) -> Result<PhaseGraphState> {
        self.validate_requested_skill(skill)?;
        if !self.phase_ids.iter().any(|id| id == phase_id) {
            return Err(GraphEngineError::UnknownPhase {
                skill: skill.to_string(),
                phase: phase_id.to_string(),
            });
        }

        let mut state = self.load_latest(session_id).await?.ok_or_else(|| {
            GraphEngineError::MissingCheckpoint {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            }
        })?;
        if state.completed_phases.iter().any(|phase| phase == phase_id) {
            return Err(GraphEngineError::PhaseAlreadyCompleted {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            });
        }
        let entry = state.dyad_verdicts.entry(phase_id.to_string()).or_default();
        if verdicts.implementer.is_some() {
            entry.implementer = verdicts.implementer;
        }
        if verdicts.reviewer_pass_by.is_some() {
            entry.reviewer_pass_by = verdicts.reviewer_pass_by;
        }
        if verdicts.tester_pass_by.is_some() {
            entry.tester_pass_by = verdicts.tester_pass_by;
        }

        self.complete_active_interrupts()?;
        self.update_state_as_node(session_id, state, phase_id).await
    }

    async fn update_state_as_node(
        &self,
        session_id: &str,
        state: PhaseGraphState,
        as_node: &str,
    ) -> Result<PhaseGraphState> {
        self.validate_phase_state_for_session(session_id, &state)?;
        let thread_id = self.thread_id(session_id)?;
        self.inner
            .update_state_as_node(&thread_id, state.clone(), as_node)
            .await
            .map_err(GraphEngineError::from_graph)?;
        self.validate_latest_checkpoint_state(session_id, &state)
            .await?;
        Ok(state)
    }

    /// The full checkpoint history for a session, oldest first. Each entry is
    /// the `PhaseGraphState` as it stood at that checkpoint. Used to project
    /// per-phase progress to local clients / JSONL streams and to locate a
    /// fork point for [`replay_phase`].
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] on checkpointer failure.
    pub async fn phase_history(&self, session_id: &str) -> Result<Vec<PhaseGraphState>> {
        Ok(self
            .phase_snapshots(session_id)
            .await?
            .into_iter()
            .map(|snapshot| snapshot.state)
            .collect())
    }

    /// The full checkpoint history with LangGraph checkpoint metadata preserved.
    ///
    /// This is the enterprise audit surface for graph execution: callers can see
    /// checkpoint ids, parent lineage, runtime source node/type, and per-checkpoint
    /// writes in addition to the reconstructed Sentinel phase state.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] on checkpointer failure.
    pub async fn phase_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<PhaseGraphCheckpointSnapshot>> {
        let thread_id = self.thread_id(session_id)?;
        let mut snapshots = self
            .inner
            .get_state_history(&thread_id)
            .await
            .map_err(GraphEngineError::from_graph)?;
        // `get_state_history` does not guarantee ordering; sort by the
        // monotonic per-thread step number so oldest is first / newest is last.
        snapshots.sort_by_key(|s| s.metadata().step_number());
        let snapshots = snapshots
            .into_iter()
            .map(Self::snapshot_view)
            .collect::<Vec<_>>();
        for snapshot in &snapshots {
            if snapshot.thread_id != thread_id {
                return Err(GraphEngineError::Graph(format!(
                    "phase graph checkpoint history for skill '{}' contains thread {}, expected {thread_id}",
                    self.workflow.skill, snapshot.thread_id
                )));
            }
            self.validate_phase_state_for_session(session_id, &snapshot.state)?;
        }
        Ok(snapshots)
    }

    fn snapshot_view(
        snapshot: CheckpointStateSnapshot<PhaseGraphState>,
    ) -> PhaseGraphCheckpointSnapshot {
        let metadata = snapshot.metadata();
        let source = metadata.source().map(|source| PhaseGraphCheckpointSource {
            step: source.step(),
            source_type: source.source_type().to_string(),
            node: source.node().map(str::to_string),
        });
        let writes = metadata
            .writes()
            .iter()
            .map(|write| PhaseGraphCheckpointWrite {
                node_id: write.node_id().to_string(),
                channel: write.channel().to_string(),
                ts: write.ts().to_rfc3339(),
            })
            .collect();
        let tags = metadata
            .tags()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();

        PhaseGraphCheckpointSnapshot {
            checkpoint_id: metadata.checkpoint_id().to_string(),
            parent_checkpoint_id: metadata.parent_checkpoint_id().map(|id| id.to_string()),
            thread_id: metadata.thread_id().to_string(),
            step_number: metadata.step_number(),
            created_at: metadata.created_at().to_rfc3339(),
            tags,
            source,
            writes,
            state: snapshot.into_state(),
        }
    }

    /// Stream LangGraph checkpoint writes for this session, oldest first.
    ///
    /// This surfaces the upstream `CheckpointSaver::get_writes_history` audit
    /// channel: callers can inspect which node wrote which checkpoint channel,
    /// the write timestamp, a content hash of the serialized value, and the
    /// decoded JSON checkpoint channel value.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] on checkpointer or write-history failure,
    /// empty write history, missing channel bytes, or non-JSON channel values.
    pub async fn phase_writes_history(
        &self,
        session_id: &str,
        channel: Option<&str>,
    ) -> Result<Vec<PhaseGraphWriteHistoryEntry>> {
        let checkpointer = self.inner.checkpointer.as_ref().ok_or_else(|| {
            GraphEngineError::Graph("phase write history requires a configured checkpointer".into())
        })?;
        let thread_id = self.thread_id(session_id)?;
        let mut stream = checkpointer.get_writes_history(&thread_id, channel);
        let mut entries = Vec::new();

        while let Some(entry) = stream.next().await {
            let entry = entry.map_err(GraphEngineError::from_graph)?;
            entries.push(Self::write_history_view(&thread_id, entry)?);
        }

        if entries.is_empty() {
            let channel_label = match channel {
                Some(channel) => format!(" channel {channel}"),
                None => String::new(),
            };
            return Err(GraphEngineError::Graph(format!(
                "phase write history is empty for thread {thread_id}{channel_label}"
            )));
        }

        if channel.is_none() && !entries.iter().any(|entry| entry.channel == "state") {
            return Err(GraphEngineError::Graph(format!(
                "phase write history for thread {thread_id} does not include state channel writes"
            )));
        }

        Ok(entries)
    }

    fn write_history_view(
        thread_id: &str,
        entry: langgraph_core::ports::WriteHistoryEntry,
    ) -> Result<PhaseGraphWriteHistoryEntry> {
        let bytes = entry.value.as_ref().ok_or_else(|| {
            GraphEngineError::Graph(format!(
                "phase write {} step {} channel {} node {} is missing its serialized value",
                entry.checkpoint_id, entry.step_number, entry.channel, entry.node_id
            ))
        })?;
        let value_json = serde_json::from_slice::<serde_json::Value>(bytes).map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase write {} step {} channel {} node {} is not valid JSON: {err}",
                entry.checkpoint_id, entry.step_number, entry.channel, entry.node_id
            ))
        })?;
        let digest = Sha256::digest(bytes);

        Ok(PhaseGraphWriteHistoryEntry {
            thread_id: thread_id.to_string(),
            checkpoint_id: entry.checkpoint_id.to_string(),
            step_number: entry.step_number,
            channel: entry.channel,
            node_id: entry.node_id,
            ts: entry.ts.to_rfc3339(),
            value_len: bytes.len(),
            value_sha256: hex::encode(digest),
            value_json,
        })
    }

    async fn validate_latest_checkpoint_state(
        &self,
        session_id: &str,
        expected_state: &PhaseGraphState,
    ) -> Result<()> {
        let snapshots = self.phase_snapshots(session_id).await?;
        let writes = self.phase_writes_history(session_id, None).await?;
        let expected_thread_id = self.thread_id(session_id)?;
        Self::validate_checkpoint_write_evidence(
            &self.workflow.skill,
            &expected_thread_id,
            expected_state,
            &snapshots,
            &writes,
        )
    }

    fn validate_checkpoint_write_evidence(
        skill: &str,
        expected_thread_id: &str,
        expected_state: &PhaseGraphState,
        snapshots: &[PhaseGraphCheckpointSnapshot],
        write_history: &[PhaseGraphWriteHistoryEntry],
    ) -> Result<()> {
        let latest = snapshots.last().ok_or_else(|| {
            GraphEngineError::Graph(format!(
                "phase graph for skill '{skill}' omitted checkpoint history for thread {expected_thread_id}"
            ))
        })?;
        if latest.thread_id != expected_thread_id {
            return Err(GraphEngineError::Graph(format!(
                "phase graph latest checkpoint thread mismatch for skill '{skill}': expected {expected_thread_id}, got {}",
                latest.thread_id
            )));
        }
        if let Some(mismatched) = snapshots
            .iter()
            .find(|snapshot| snapshot.thread_id != expected_thread_id)
        {
            return Err(GraphEngineError::Graph(format!(
                "phase graph checkpoint history for skill '{skill}' contains thread {}, expected {expected_thread_id}",
                mismatched.thread_id
            )));
        }
        for pair in snapshots.windows(2) {
            if pair[0].step_number > pair[1].step_number {
                return Err(GraphEngineError::Graph(format!(
                    "phase graph checkpoint history for skill '{skill}' thread {expected_thread_id} is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                    pair[0].checkpoint_id,
                    pair[0].step_number,
                    pair[1].checkpoint_id,
                    pair[1].step_number
                )));
            }
        }
        if let Some(mismatched) = write_history
            .iter()
            .find(|write| write.thread_id != expected_thread_id)
        {
            return Err(GraphEngineError::Graph(format!(
                "phase graph write history for skill '{skill}' contains thread {}, expected {expected_thread_id}",
                mismatched.thread_id
            )));
        }
        for pair in write_history.windows(2) {
            if pair[0].step_number > pair[1].step_number {
                return Err(GraphEngineError::Graph(format!(
                    "phase graph write history for skill '{skill}' thread {expected_thread_id} is not oldest-first: checkpoint '{}' step {} appears before checkpoint '{}' step {}",
                    pair[0].checkpoint_id,
                    pair[0].step_number,
                    pair[1].checkpoint_id,
                    pair[1].step_number
                )));
            }
        }
        if latest.state != *expected_state {
            return Err(GraphEngineError::Graph(format!(
                "phase graph latest checkpoint state mismatch for skill '{skill}' thread {expected_thread_id}"
            )));
        }

        let expected_json = serde_json::to_value(expected_state).map_err(|err| {
            GraphEngineError::Graph(format!(
                "phase graph failed to serialize expected state for skill '{skill}' thread {expected_thread_id}: {err}"
            ))
        })?;
        let latest_state_writes: Vec<_> = latest
            .writes
            .iter()
            .filter(|write| write.channel == "state")
            .collect();
        if latest_state_writes.is_empty() {
            return Err(GraphEngineError::Graph(format!(
                "phase graph latest checkpoint '{}' omitted state-channel write metadata for skill '{skill}' thread {expected_thread_id}",
                latest.checkpoint_id
            )));
        }

        let latest_write = write_history
            .iter()
            .find(|write| {
                write.checkpoint_id == latest.checkpoint_id
                    && write.channel == "state"
                    && latest_state_writes
                        .iter()
                        .any(|metadata| metadata.node_id == write.node_id && metadata.ts == write.ts)
            })
            .ok_or_else(|| {
                GraphEngineError::Graph(format!(
                    "phase graph write history omitted latest state-channel write for checkpoint '{}' skill '{skill}' thread {expected_thread_id}",
                    latest.checkpoint_id
                ))
            })?;
        if &latest_write.value_json != &expected_json {
            return Err(GraphEngineError::Graph(format!(
                "phase graph latest state-channel write mismatch for checkpoint '{}' skill '{skill}' thread {expected_thread_id}",
                latest.checkpoint_id
            )));
        }

        Ok(())
    }

    /// Time-travel: re-attempt `phase_id` by forking from the checkpoint as it
    /// stood *before* that phase was last completed.
    ///
    /// Finds the most recent checkpoint whose `completed_phases` does NOT yet
    /// contain `phase_id` (the state just before that phase passed), rewinds
    /// `current_phase` to that phase with a fresh [`Verdict::Pending`], and
    /// persists it as a new checkpoint — a fork in the history tree. The phase
    /// can then be re-run and re-judged. This realises the QA-failed
    /// re-attempt as first-class `LangGraph` time-travel (replay/fork).
    ///
    /// # Errors
    /// Returns [`GraphEngineError::UnknownPhase`] if `phase_id` is not part of
    /// this workflow, [`GraphEngineError::InvalidReplayReason`] if `reason` is
    /// blank, [`GraphEngineError::MissingCheckpoint`] if there is no checkpoint
    /// lineage to fork, [`GraphEngineError::PhaseNotCompletedForReplay`] if
    /// the latest checkpoint has not completed `phase_id`, or on checkpointer
    /// failure.
    pub async fn replay_phase(
        &self,
        skill: &str,
        session_id: &str,
        phase_id: &str,
        reason: &str,
    ) -> Result<PhaseGraphState> {
        self.validate_requested_skill(skill)?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(GraphEngineError::InvalidReplayReason {
                skill: skill.to_string(),
                phase: phase_id.to_string(),
            });
        }
        let idx = self
            .phase_ids
            .iter()
            .position(|p| p == phase_id)
            .ok_or_else(|| GraphEngineError::UnknownPhase {
                skill: skill.to_string(),
                phase: phase_id.to_string(),
            })?;

        let history = self.phase_history(session_id).await?;
        let latest =
            history
                .last()
                .cloned()
                .ok_or_else(|| GraphEngineError::MissingCheckpoint {
                    skill: skill.to_string(),
                    session_id: session_id.to_string(),
                    phase: phase_id.to_string(),
                })?;
        if !latest
            .completed_phases
            .iter()
            .any(|completed| completed == phase_id)
        {
            return Err(GraphEngineError::PhaseNotCompletedForReplay {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            });
        }
        let superseded_completed_phases = latest
            .completed_phases
            .iter()
            .filter(|phase| {
                self.phase_ids
                    .iter()
                    .position(|candidate| candidate == *phase)
                    .is_some_and(|completed_idx| completed_idx >= idx)
            })
            .cloned()
            .collect();
        let superseded_step_states = latest
            .step_states
            .iter()
            .filter(|step| {
                self.phase_ids
                    .iter()
                    .position(|phase| phase == &step.phase_id)
                    .is_some_and(|step_idx| step_idx >= idx)
            })
            .map(PhaseReplayStepState::from)
            .collect();
        // Most recent checkpoint that predates this phase's completion.
        let mut fork = history
            .into_iter()
            .rev()
            .find(|s| {
                s.completed_phases.iter().all(|completed| {
                    self.phase_ids
                        .iter()
                        .position(|x| x == completed)
                        .is_some_and(|completed_idx| completed_idx < idx)
                })
            })
            .ok_or_else(|| GraphEngineError::MissingCheckpoint {
                skill: skill.to_string(),
                session_id: session_id.to_string(),
                phase: phase_id.to_string(),
            })?;

        // Rewind to the phase being replayed, dropping it and anything after
        // from the completed set so the re-run is judged fresh.
        fork.completed_phases.retain(|p| {
            self.phase_ids
                .iter()
                .position(|x| x == p)
                .is_some_and(|completed_idx| completed_idx < idx)
        });
        fork.step_states.retain(|step| {
            self.phase_ids
                .iter()
                .position(|phase| phase == &step.phase_id)
                .is_some_and(|step_idx| step_idx < idx)
        });
        fork.step_policy_evidence.retain(|policy| {
            fork.step_states
                .iter()
                .any(|step| step.phase_id == policy.phase_id && step.step_id == policy.step_id)
        });
        if fork
            .current_step
            .as_ref()
            .is_some_and(|current| !fork.step_states.iter().any(|step| &step.step_id == current))
        {
            fork.current_step = None;
        }
        fork.current_phase = Some(idx);
        fork.complete = false;
        fork.last_verdict = Verdict::Pending;
        fork.replay_events.clone_from(&latest.replay_events);
        fork.replay_events.push(PhaseReplayEvent {
            phase_id: phase_id.to_string(),
            reason: reason.to_string(),
            superseded_completed_phases,
            superseded_step_states,
        });

        self.update_state_as_node(session_id, fork, START).await
    }
}
