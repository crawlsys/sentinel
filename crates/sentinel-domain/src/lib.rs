//! Sentinel Domain Layer
//!
//! Pure business logic with no IO dependencies.
//! Defines proof chains, workflows, evidence, hooks, and routing.

pub mod constants;
pub mod dependency;
pub mod events;
pub mod interceptor;
pub mod evidence;
pub mod hooks;
pub mod judge;
pub mod ports;
pub mod project;
pub mod proof;
pub mod routing;
pub mod session;
pub mod state;
pub mod workflow;

// Re-export commonly used types
pub use events::HookEvent;
pub use evidence::{Evidence, EvidenceEntry};
pub use hooks::{HookId, HookResult, HookSpec};
pub use judge::JudgeVerdict;
pub use proof::{PhaseProof, ProofChain};
pub use routing::RegexRouter;
pub use session::SessionId;
pub use state::SessionState;
pub use workflow::{SkillWorkflow, WorkflowPhase};
