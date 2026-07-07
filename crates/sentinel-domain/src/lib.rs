//! Sentinel Domain Layer
//!
//! Pure business logic with no IO dependencies.
//! Defines proof chains, workflows, evidence, hooks, and routing.

pub mod agent_routing;
pub mod ba;
pub mod capability;
pub mod commit;
pub mod config;
pub mod constants;
pub mod dependency;
pub mod disagreement;
pub mod dry_run;
pub mod error_classifier;
pub mod eval;
pub mod events;
pub mod evidence;
pub mod evidence_adapter;
pub mod exchange;
pub mod file_kind;
pub mod hooks;
pub mod interceptor;
pub mod judge;
pub mod langgraph_thread;
pub mod mcp_tool;
pub mod multi_judge;
pub mod override_phrase;
pub mod path_safety;
pub mod paths;
pub mod port_errors;
pub mod ports;
pub mod pricing;
pub mod project;
pub mod proof;
pub mod repo_kind;
pub mod request_limits;
pub mod reversibility;
pub mod review;
pub mod routing;
pub mod session;
pub mod spec_challenge;
pub mod ssrf;
pub mod state;
pub mod step_manifest;
pub mod step_proof;
pub mod step_verifier;
pub mod task_decoration;
pub mod test_evidence;
pub mod tracing;
pub mod workflow;

// Re-export commonly used types
pub use dry_run::{AuditorAxes, AuditorDecision, AuditorError, AuditorVerdict, DryRunRequest};
pub use events::{HookEnvelope, HookEvent, HookTier};
pub use evidence::{Evidence, EvidenceEntry};
pub use evidence_adapter::{
    compute_payload_hash, compute_provenance_hash, AdapterError, EvidenceClaim, EvidenceReceipt,
};
pub use hooks::{HookId, HookResult, HookSpec};
pub use judge::JudgeVerdict;
pub use pricing::{cost_for, short_model_label, tier_for_model, PricingTier, TokenUsage};
pub use proof::{PhaseProof, ProofChain};
pub use request_limits::{CallWindow, LimitError, RequestLimits};
pub use reversibility::{ParseReversibilityClassError, ReversibilityClass};
pub use routing::RegexRouter;
pub use session::SessionId;
pub use state::SessionState;
pub use step_proof::StepProof;
pub use tracing::{TraceContext, TraceParseError};
pub use workflow::{SkillWorkflow, WorkflowPhase};
