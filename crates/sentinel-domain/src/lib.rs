//! Sentinel Domain Layer
//!
//! Pure business logic with no IO dependencies.
//! Defines proof chains, workflows, evidence, hooks, and routing.

pub mod commit;
pub mod constants;
pub mod dependency;
pub mod error_classifier;
pub mod events;
pub mod evidence;
pub mod exchange;
pub mod file_kind;
pub mod hooks;
pub mod interceptor;
pub mod judge;
pub mod mcp_tool;
pub mod override_phrase;
pub mod path_safety;
pub mod ports;
pub mod pricing;
pub mod project;
pub mod proof;
pub mod repo_kind;
pub mod routing;
pub mod session;
pub mod state;
pub mod workflow;

// Re-export commonly used types
pub use events::{HookEnvelope, HookEvent, HookTier};
pub use evidence::{Evidence, EvidenceEntry};
pub use hooks::{HookId, HookResult, HookSpec};
pub use judge::JudgeVerdict;
pub use pricing::{cost_for, short_model_label, tier_for_model, PricingTier, TokenUsage};
pub use proof::{PhaseProof, ProofChain};
pub use routing::RegexRouter;
pub use session::SessionId;
pub use state::SessionState;
pub use workflow::{SkillWorkflow, WorkflowPhase};
