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
//!   call* and checkpoints to a sqlite [`CheckpointSaver`] keyed by
//!   `thread_id = session_id`. Process death between calls is fine:
//!   `load_latest` restores. This is `LangGraph`'s durable-execution model and
//!   it is a near-exact match for sentinel's invocation model.
//! - **Judge-as-interrupt** — every phase node interrupts *after* execution
//!   (`InterruptConfig::after()`); the out-of-process judge verdict is fed
//!   back via [`Command::resume_with`], making the gate structural rather
//!   than a bolted-on side effect.
//! - **Time-travel** — a QA-failed re-attempt forks from the checkpoint
//!   *before* the failed phase (`get_state_history` + `update_state`).
//!
//! ## Boundary
//!
//! This crate owns the langgraph dependency so the sync hot-path hook
//! (`phase_gate`) never touches it. The graph lives only in the **async MCP
//! server**. The graph's state type ([`PhaseGraphState`]) is graph-local and
//! converts to/from the domain [`WorkflowState`], keeping `sentinel-domain`
//! free of any langgraph dependency (hexagonal boundary).
//!
//! [`CheckpointSaver`]: langgraph_core::ports::CheckpointSaver
//! [`Command::resume_with`]: langgraph_core::domain::value_objects::Command::resume_with
//! [`WorkflowState`]: sentinel_domain::workflow::WorkflowState

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use langgraph_core::application::services::{CompilationResult, GraphCompiler};
use langgraph_core::domain::value_objects::{InterruptConfig, NodeError, END, START};
use langgraph_core::ports::CheckpointSaver;
use langgraph_core::{SqliteCheckpointer, StateGraphBuilder};

use sentinel_domain::workflow::{SkillWorkflow, WorkflowState};

mod error;
pub use error::GraphEngineError;

#[cfg(test)]
mod tests;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, GraphEngineError>;

/// The verdict a judge returns for a phase. Serializable so it survives the
/// checkpoint round-trip: the phase node records it, the conditional edge
/// reads it after `resume`.
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
    /// The verdict for the phase at `current_phase`. Set by the resume value,
    /// read by the conditional edge.
    pub last_verdict: Verdict,
}

impl PhaseGraphState {
    /// Initial state for a skill's workflow run.
    #[must_use]
    pub fn new(skill: impl Into<String>, session_id: impl Into<String>, phase_order: Vec<String>) -> Self {
        Self {
            skill: skill.into(),
            session_id: session_id.into(),
            phase_order,
            current_phase: None,
            completed_phases: Vec::new(),
            complete: false,
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
        ws
    }

    /// Hydrate graph state from a domain [`WorkflowState`] plus the phase
    /// order from the workflow definition.
    #[must_use]
    pub fn from_workflow_state(ws: &WorkflowState, phase_order: Vec<String>) -> Self {
        Self {
            skill: ws.skill.clone(),
            session_id: ws.session_id.clone(),
            phase_order,
            current_phase: ws.current_phase,
            completed_phases: ws.completed_phases.clone(),
            complete: ws.complete,
            last_verdict: Verdict::Pending,
        }
    }
}

/// Build a durable sqlite [`CheckpointSaver`] for a session. The database path
/// is the caller's responsibility (it lives under sentinel's state dir); pass
/// `":memory:"` for tests.
///
/// # Errors
/// Returns [`GraphEngineError::Checkpointer`] if the sqlite pool/schema cannot
/// be initialised.
pub async fn phase_saver(database_path: &str) -> Result<Arc<dyn CheckpointSaver>> {
    let saver = SqliteCheckpointer::new(database_path)
        .await
        .map_err(GraphEngineError::from_graph)?;
    Ok(Arc::new(saver))
}

/// Compile a [`SkillWorkflow`] into a checkpointed phase graph.
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
pub fn compile_skill_graph(
    workflow: &SkillWorkflow,
    checkpointer: Arc<dyn CheckpointSaver>,
) -> Result<CompiledPhaseGraph> {
    if workflow.phases.is_empty() {
        return Err(GraphEngineError::EmptyWorkflow(workflow.skill.clone()));
    }

    let phase_ids: Vec<String> = workflow.phases.iter().map(|p| p.id.clone()).collect();
    let mut builder = StateGraphBuilder::<PhaseGraphState>::new();

    // One node per phase. The node marks the phase active (records its index)
    // and is the interrupt point — actual phase *work* happens in the agent,
    // out of process; the node only advances bookkeeping.
    for (idx, phase_id) in phase_ids.iter().enumerate() {
        let this_phase = phase_id.clone();
        builder = builder.add_node(phase_id.as_str(), move |state: &PhaseGraphState| {
            let mut next = state.clone();
            next.current_phase = Some(idx);
            // The node itself does not decide Pass/Fail; the verdict arrives
            // via resume. Reset to Pending so a stale verdict can't leak across
            // phases.
            next.last_verdict = Verdict::Pending;
            let _ = &this_phase;
            Ok::<_, NodeError>(next)
        });
    }

    builder = builder.add_edge(START, phase_ids[0].as_str());

    // Conditional routing per phase: delegate to the pure `next_phase_target`
    // so the decision is unit-testable without driving the executor.
    for (idx, phase_id) in phase_ids.iter().enumerate() {
        let order = phase_ids.clone();
        builder = builder.add_conditional_edge(phase_id.as_str(), move |state: &PhaseGraphState| {
            next_phase_target(state.last_verdict, idx, &order).into()
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
    // bookkeeping node, then pauses so the out-of-process judge can supply a
    // verdict via `resume`. `with_interrupt` is a builder on the compiled
    // graph, so re-fold it through the publicly-exposed `graph` field.
    let mut compiled = compilation.graph;
    for phase_id in &phase_ids {
        compiled = compiled.with_interrupt(phase_id.as_str(), InterruptConfig::after());
    }
    compilation.graph = compiled;

    Ok(CompiledPhaseGraph {
        inner: compilation,
        phase_ids,
    })
}

/// Pure routing decision for the conditional edge leaving the phase at
/// `phase_idx`, given the judge `verdict` and the workflow's `phase_order`.
///
/// - `Pass` → the next phase id, or [`END`] if this was the last phase.
/// - `Fail` / `Pending` → the same phase id (loop back / stay paused).
///
/// This is the single source of truth for phase routing: the compiled graph's
/// conditional edges delegate here, and the MCP handler uses it to know what
/// to checkpoint after a verdict.
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

/// A compiled, checkpointed phase graph plus the ordered phase ids it was
/// built from. Wraps `langgraph-core`'s `CompilationResult` (which carries the
/// checkpointer for durable execution + time-travel).
pub struct CompiledPhaseGraph {
    inner: CompilationResult<PhaseGraphState>,
    phase_ids: Vec<String>,
}

impl CompiledPhaseGraph {
    /// The ordered phase ids of this workflow.
    #[must_use]
    pub fn phase_ids(&self) -> &[String] {
        &self.phase_ids
    }

    /// Borrow the underlying compilation result (for execute/resume/time-travel
    /// via the `ExecutableGraph` extension trait).
    #[must_use]
    pub fn inner(&self) -> &CompilationResult<PhaseGraphState> {
        &self.inner
    }

    /// Load the latest checkpointed state for a session, if any.
    ///
    /// # Errors
    /// Returns [`GraphEngineError`] on checkpointer failure.
    pub async fn load_latest(&self, session_id: &str) -> Result<Option<PhaseGraphState>> {
        self.inner
            .get_state(session_id)
            .await
            .map_err(GraphEngineError::from_graph)
    }
}
