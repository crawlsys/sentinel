//! A12 — External-benchmark eval domain types.
//!
//! Per `docs/a12-external-benchmarks.md` §3. Pure value objects for
//! the BA-Eval corpus + `TheAgentCompany` run records. Phase 1 (this
//! module) ships the **case + rubric + score types**; the
//! `sentinel eval` CLI subcommand, corpus storage adapter, and
//! actual benchmark runner land in later phases.
//!
//! ## R5 quarantine boundary
//!
//! Per `docs/policy-replay-mining-quarantine.md`: eval scores feed
//! operator dashboards, A2 appraisal counters (as deterministic
//! dispatch input), and methodology decisions. Scores do NOT feed
//! agent training pipelines or auto-promotion of prompt variants.
//! The corpus has a private test split (not modeled at the type
//! level; enforced by the storage adapter's directory layout per
//! spec §3.4) that agents are never exposed to during prompt
//! iteration.

pub mod case;
pub mod rubric;

pub use case::{
    CaseProvenance, EvalCase, EvalCaseId, EvalRunId, GoldArtifact, GoldOutcomes, RedactionLevel,
    SourceCorpus,
};
pub use rubric::{EvalAxis, EvalAxisScore, EvalScore, ScoringRubric};
