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
pub mod evidence_adapter;
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
pub mod request_limits;
pub mod step_proof;
pub mod routing;
pub mod session;
pub mod state;
pub mod test_evidence;
pub mod tracing;
pub mod workflow;

// Re-export commonly used types
pub use events::{HookEnvelope, HookEvent, HookTier};
pub use evidence::{Evidence, EvidenceEntry};
pub use evidence_adapter::{
    compute_payload_hash, compute_provenance_hash, AdapterError, EvidenceClaim,
    EvidenceReceipt,
};
pub use hooks::{HookId, HookResult, HookSpec};
pub use judge::JudgeVerdict;
pub use pricing::{cost_for, short_model_label, tier_for_model, PricingTier, TokenUsage};
pub use proof::{PhaseProof, ProofChain};
pub use request_limits::{CallWindow, LimitError, RequestLimits};
pub use step_proof::StepProof;
pub use routing::RegexRouter;
pub use tracing::{TraceContext, TraceParseError};
pub use session::SessionId;
pub use state::SessionState;
pub use workflow::{SkillWorkflow, WorkflowPhase};
