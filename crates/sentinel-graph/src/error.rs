//! Error type for the phase-graph engine.

use langgraph_core::domain::value_objects::GraphError;

/// Errors raised when compiling or driving a sentinel phase graph.
#[derive(Debug, thiserror::Error)]
pub enum GraphEngineError {
    /// The workflow definition had no phases — nothing to compile.
    #[error("workflow '{0}' has no phases")]
    EmptyWorkflow(String),

    /// A verdict was submitted for a phase id not present in the workflow.
    #[error("phase '{phase}' is not part of skill '{skill}' workflow")]
    UnknownPhase {
        /// Skill whose workflow was queried.
        skill: String,
        /// The unknown phase id.
        phase: String,
    },

    /// A phase verdict was submitted before an earlier required phase passed.
    #[error(
        "phase '{phase}' for skill '{skill}' cannot advance before required prior phase '{missing}'"
    )]
    PhaseOrderViolation {
        /// Skill whose workflow was queried.
        skill: String,
        /// Phase whose verdict was submitted.
        phase: String,
        /// Prior required phase that is still incomplete.
        missing: String,
    },

    /// A phase has a reviewer/tester dyad requirement that is not satisfied.
    #[error(
        "phase '{phase}' for skill '{skill}' cannot advance until its required role dyad passes"
    )]
    DyadUnsatisfied {
        /// Skill whose workflow was queried.
        skill: String,
        /// Phase whose verdict was submitted.
        phase: String,
    },

    /// A verdict targeted a phase that is already complete in the latest
    /// checkpoint. Re-running sealed work must fork via replay/time-travel so
    /// the superseded history is auditable.
    #[error(
        "phase '{phase}' for skill '{skill}' is already completed in session '{session_id}'; replay the phase before changing sealed phase state"
    )]
    PhaseAlreadyCompleted {
        /// Skill whose workflow was queried.
        skill: String,
        /// Session whose latest graph state was queried.
        session_id: String,
        /// Phase whose verdict was submitted.
        phase: String,
    },

    /// A step status update targeted a step that is already terminal in the
    /// latest checkpoint. Rewriting sealed step state must fork via
    /// replay/time-travel so the superseded history is auditable.
    #[error(
        "step '{step_id}' in phase '{phase}' for skill '{skill}' is already terminal with status '{status}' in session '{session_id}'; replay the phase before changing sealed step state"
    )]
    StepAlreadyTerminal {
        /// Skill whose workflow was queried.
        skill: String,
        /// Session whose latest graph state was queried.
        session_id: String,
        /// Phase whose step was updated.
        phase: String,
        /// Step whose status was updated.
        step_id: String,
        /// Existing terminal status in the latest checkpoint.
        status: String,
    },

    /// A graph operation required durable checkpoint history but none existed.
    #[error(
        "phase '{phase}' for skill '{skill}' requires an existing checkpoint in session '{session_id}'"
    )]
    MissingCheckpoint {
        /// Skill whose workflow was queried.
        skill: String,
        /// Session whose graph history was queried.
        session_id: String,
        /// Phase whose checkpoint lineage was required.
        phase: String,
    },

    /// A replay/time-travel request did not include the operator intent needed
    /// to audit the forked history.
    #[error("phase '{phase}' for skill '{skill}' cannot be replayed without a non-empty reason")]
    InvalidReplayReason {
        /// Skill whose workflow was queried.
        skill: String,
        /// Phase whose replay was requested.
        phase: String,
    },

    /// A replay/time-travel request targeted a phase that is not completed in
    /// the latest graph checkpoint, so there is no sealed progress to supersede.
    #[error(
        "phase '{phase}' for skill '{skill}' cannot be replayed because it is not completed in session '{session_id}'"
    )]
    PhaseNotCompletedForReplay {
        /// Skill whose workflow was queried.
        skill: String,
        /// Session whose latest graph state was queried.
        session_id: String,
        /// Phase whose replay was requested.
        phase: String,
    },

    /// The sqlite checkpointer could not be initialised.
    #[error("checkpointer error: {0}")]
    Checkpointer(String),

    /// Compiled graph topology evidence is missing or internally inconsistent.
    #[error("topology error: {0}")]
    Topology(String),

    /// An error bubbled up from `langgraph-core` (compile, execute, resume,
    /// or checkpoint I/O). Flattened to a string so this crate's public API
    /// does not leak the langgraph error type.
    #[error("graph engine error: {0}")]
    Graph(String),
}

impl GraphEngineError {
    /// Wrap a `langgraph-core` [`GraphError`] as a flattened engine error.
    pub(crate) fn from_graph(err: GraphError) -> Self {
        Self::Graph(err.to_string())
    }
}
