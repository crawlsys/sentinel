//! BA-vertical domain types.
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §9. Pure value objects
//! and check-result enums for the two S-tier structural disciplines
//! that distinguish the AI-factory BA product from a generic LLM:
//!
//! - **BA1** (citation-locked decision artifacts) — every claim
//!   source-pinned via an [`ArtifactReference`]; validated via the
//!   `provenance_validate` hook (future phase).
//! - **BA3** (requirements traceability matrix) — every recommendation
//!   traces to a [`RequirementRef`]; validated via the
//!   `requirements_traceability_gate` hook (future phase).
//!
//! Phase 1 (this module) ships the **wire-format types** + the
//! **check / finding** enums. Hooks, ports, and adapters land in
//! later phases.

pub mod provenance;
pub mod requirements;

pub use provenance::{
    ArtifactReference, ProvenanceCheck, ProvenanceClass, ProvenanceFinding, RetrievalRecord,
};
pub use requirements::{RequirementCheck, RequirementFinding, RequirementRef};
