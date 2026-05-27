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

    /// The sqlite checkpointer could not be initialised.
    #[error("checkpointer error: {0}")]
    Checkpointer(String),

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
