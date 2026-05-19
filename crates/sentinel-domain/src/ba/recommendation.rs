//! BA-orchestrator output envelope.
//!
//! The downstream emitter that sentinel's BA1 / BA3 / A13 gates
//! were designed to verify. Until something produces this envelope,
//! the gates are structurally inert — every prior gate ships the
//! constraint side; this module ships the artifact side.
//!
//! ## What lives here vs. what doesn't
//!
//! - **Here (Phase 1)**: data shapes for the draft request + the
//!   recommendation output, plus the `StakeholderAudience` enum.
//!   Pure domain — no LLM call, no IO, no ports.
//! - **Phase 2 (application)**: the orchestrator use case
//!   (`ba_orchestrator::draft(request) -> BaRecommendation`) that
//!   wires connector pulls + LLM synthesis + spec-challenge
//!   emission into one flow.
//! - **Phase 3 (CLI / MCP)**: operator-facing surface (`sentinel
//!   ba draft` and/or an MCP tool `ba_draft_recommendation`).
//!
//! ## Why this envelope shape
//!
//! Every field maps directly to a hook input field consumed by a
//! sentinel gate already on disk:
//!
//! | Field | Consumer |
//! |---|---|
//! | `citations: Vec<ArtifactReference>` | BA1 `provenance_validate` reads `extra.artifacts` |
//! | `requirement_refs: Vec<RequirementRef>` | BA3 `requirements_traceability_gate` reads `extra.requirement_refs` |
//! | `spec_challenge: SpecChallenge` | A13 `spec_challenge_gate` reads `extra.spec_challenge` |
//!
//! So when the orchestrator emits a [`BaRecommendation`], serializing
//! its fields into the next tool call's `extra` map automatically
//! satisfies the gates. The orchestrator doesn't need to know about
//! sentinel's internal hook plumbing — it just produces the
//! recommendation in this shape.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ba::provenance::ArtifactReference;
use crate::ba::requirements::RequirementRef;
use crate::spec_challenge::SpecChallenge;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Opaque identifier for a single recommendation draft.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecommendationId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecommendationIdError {
    Empty,
}

impl std::fmt::Display for RecommendationIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "recommendation_id cannot be empty"),
        }
    }
}

impl std::error::Error for RecommendationIdError {}

impl RecommendationId {
    /// Construct, rejecting empty / whitespace-only inputs.
    pub fn new(s: impl Into<String>) -> Result<Self, RecommendationIdError> {
        let trimmed = s.into().trim().to_string();
        if trimmed.is_empty() {
            return Err(RecommendationIdError::Empty);
        }
        Ok(Self(trimmed))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RecommendationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Stakeholder audience
// ---------------------------------------------------------------------------

/// Who the recommendation is for. Drives the
/// [`A12 EvalAxis::StakeholderFit`](crate::eval::EvalAxis::StakeholderFit)
/// dimension when scored, and the BA-orchestrator's prompt
/// scaffolding when drafted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StakeholderAudience {
    /// C-suite single decision-maker; expects tight reasoning,
    /// explicit recommendation, blast-radius framing.
    Exec,
    /// Board memo; expects governance framing, risk register, and
    /// substantive treatment of alternatives.
    Board,
    /// External customer-facing brief; expects translated jargon,
    /// concrete next steps, no internal speculation.
    Customer,
    /// Engineering or ops team peer; expects implementation
    /// specifics, technical tradeoffs, and direct constraints.
    InternalTeam,
}

impl StakeholderAudience {
    /// Stable string identifier for serialization + reporting.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Board => "board",
            Self::Customer => "customer",
            Self::InternalTeam => "internal_team",
        }
    }
}

// ---------------------------------------------------------------------------
// BaDraftRequest
// ---------------------------------------------------------------------------

/// Operator-supplied input to a BA recommendation draft.
///
/// Either the CLI or the MCP tool surface accepts this and hands it
/// to the orchestrator. The orchestrator pulls supporting evidence
/// from connectors, synthesizes a recommendation, and emits a
/// [`BaRecommendation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaDraftRequest {
    /// The stakeholder's brief in their own words. The
    /// orchestrator's first job is to re-state this internally
    /// (per A13's spec-challenge contract).
    pub brief: String,
    /// Who the output is for. Drives both tone and the
    /// stakeholder-fit axis when the result is benchmarked.
    pub stakeholder_audience: StakeholderAudience,
    /// Operator-stated constraints the orchestrator must honor or
    /// explicitly surface as `constraints_not_satisfied` in the
    /// emitted [`SpecChallenge`]. Examples: `"no PII in output"`,
    /// `"must cite at least 3 distinct sources"`, `"avoid
    /// vendor-specific recommendations"`.
    pub constraints: Vec<String>,
}

impl BaDraftRequest {
    /// Whitespace-only briefs are rejected — the orchestrator can't
    /// draft against silence. Empty `constraints` is fine.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        !self.brief.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// BaRecommendation
// ---------------------------------------------------------------------------

/// The BA-orchestrator's output envelope.
///
/// Every field maps to a sentinel-gate input. Once an orchestrator
/// emits one of these and serializes its fields into the `extra`
/// payload of the next downstream tool call, BA1 / BA3 / A13
/// gates verify the output structurally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaRecommendation {
    pub recommendation_id: RecommendationId,
    /// Echo of the original brief — kept so audit consumers can
    /// re-verify the brief→recommendation pairing without
    /// chasing the draft request.
    pub brief: String,
    pub stakeholder_audience: StakeholderAudience,
    /// The recommendation body (the human-facing prose the BA
    /// would ship). Plain text or markdown; no embedded HTML.
    pub body: String,
    /// Every cited source. BA1 `provenance_validate` reads these
    /// as `extra.artifacts` and validates each against the
    /// connector audit chain.
    pub citations: Vec<ArtifactReference>,
    /// Every requirement this recommendation claims to address.
    /// BA3 `requirements_traceability_gate` reads these as
    /// `extra.requirement_refs` and verifies the matrix has a
    /// matching row at the cited content hash.
    pub requirement_refs: Vec<RequirementRef>,
    /// The agent's pre-action self-examination. A13
    /// `spec_challenge_gate` reads this as `extra.spec_challenge`
    /// and runs the completeness check (and, at Catastrophic
    /// class, the semantic-quality scorer).
    pub spec_challenge: SpecChallenge,
    pub generated_at: DateTime<Utc>,
    /// Free-form identifier for the agent that produced the
    /// recommendation. Sentinel doesn't validate this is in the
    /// agent registry — the field exists for audit attribution.
    pub agent_id: String,
}

impl BaRecommendation {
    /// `true` when the recommendation has at least one citation,
    /// at least one `requirement_ref`, and a structurally-complete
    /// `spec_challenge`. This is the *structural* readiness check —
    /// the actual gate verdicts depend on the citation matching
    /// the audit chain, the matrix matching, and (for Catastrophic
    /// class) the spec-challenge scorer.
    #[must_use]
    pub fn is_structurally_ready_for_publication(&self) -> bool {
        !self.citations.is_empty()
            && !self.requirement_refs.is_empty()
            && self.spec_challenge.is_complete()
    }

    /// Number of distinct sources cited. Operators tune
    /// "minimum cite-count" constraints against this.
    #[must_use]
    pub fn distinct_citation_count(&self) -> usize {
        use std::collections::HashSet;
        self.citations
            .iter()
            .map(|c| c.artifact_id.as_str())
            .collect::<HashSet<_>>()
            .len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ba::provenance::ProvenanceClass;
    use crate::reversibility::ReversibilityClass;
    use crate::spec_challenge::{
        Alternative, Ambiguity, Assumption, AssumptionConfidence, ChallengeCategory,
        GapResolution, SpecChallenge, SpecGap, SpecReference, WorkId,
    };
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn complete_challenge() -> SpecChallenge {
        SpecChallenge {
            work_id: WorkId::new("w1").unwrap(),
            agent_id: "ba-orchestrator".to_string(),
            challenged_spec: SpecReference {
                hash: "abc".to_string(),
                source: "stakeholder brief".to_string(),
            },
            reversibility_class: ReversibilityClass::Irreversible,
            assumptions: ChallengeCategory::new(vec![Assumption {
                statement: "stakeholder cares about p99 latency".to_string(),
                confidence: AssumptionConfidence::Medium,
                blast_if_wrong: ReversibilityClass::ReversibleWithEffort,
            }]),
            gaps: ChallengeCategory::new(vec![SpecGap {
                topic: "budget".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            }]),
            ambiguities: ChallengeCategory::new(vec![Ambiguity {
                spec_excerpt: "scale up".to_string(),
                interpretations: vec!["10x traffic".to_string(), "10x users".to_string()],
                chosen: "10x users".to_string(),
                rationale: "stakeholder context implies user growth".to_string(),
            }]),
            alternatives_considered: ChallengeCategory::new(vec![Alternative {
                description: "vertical scaling".to_string(),
                why_rejected: "operator ruled it out".to_string(),
            }]),
            constraints_not_satisfied: ChallengeCategory::none("all constraints satisfied"),
            created_at: ts(),
        }
    }

    fn artifact(id: &str) -> ArtifactReference {
        ArtifactReference {
            artifact_id: id.to_string(),
            content_hash: format!("hash-{id}"),
            provenance_class: ProvenanceClass::SystemOfRecord,
            retrieved_at: ts(),
        }
    }

    fn requirement(orchestration_id: &str, row_id: &str) -> RequirementRef {
        RequirementRef {
            orchestration_id: orchestration_id.to_string(),
            matrix_row_id: row_id.to_string(),
            content_hash: format!("h-{row_id}"),
            statement: format!("requirement {row_id}"),
        }
    }

    fn make_recommendation() -> BaRecommendation {
        BaRecommendation {
            recommendation_id: RecommendationId::new("rec-1").unwrap(),
            brief: "scale the platform".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            body: "Recommendation: horizontally scale the API tier...".to_string(),
            citations: vec![artifact("linear://issue/FPCRM-42")],
            requirement_refs: vec![requirement("orch-1", "row-1")],
            spec_challenge: complete_challenge(),
            generated_at: ts(),
            agent_id: "ba-orchestrator".to_string(),
        }
    }

    #[test]
    fn recommendation_id_rejects_empty() {
        assert!(RecommendationId::new("").is_err());
        assert!(RecommendationId::new("   ").is_err());
    }

    #[test]
    fn recommendation_id_trims_whitespace() {
        let id = RecommendationId::new("  rec-1  ").unwrap();
        assert_eq!(id.as_str(), "rec-1");
    }

    #[test]
    fn stakeholder_audience_keys_are_snake_case() {
        assert_eq!(StakeholderAudience::Exec.key(), "exec");
        assert_eq!(StakeholderAudience::Board.key(), "board");
        assert_eq!(StakeholderAudience::Customer.key(), "customer");
        assert_eq!(StakeholderAudience::InternalTeam.key(), "internal_team");
    }

    #[test]
    fn draft_request_well_formed_requires_brief() {
        let request = BaDraftRequest {
            brief: "real brief".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            constraints: vec![],
        };
        assert!(request.is_well_formed());

        let blank = BaDraftRequest {
            brief: "   ".to_string(),
            stakeholder_audience: StakeholderAudience::Exec,
            constraints: vec![],
        };
        assert!(!blank.is_well_formed());
    }

    #[test]
    fn draft_request_well_formed_allows_empty_constraints() {
        let request = BaDraftRequest {
            brief: "brief".to_string(),
            stakeholder_audience: StakeholderAudience::Board,
            constraints: vec![],
        };
        assert!(request.is_well_formed());
    }

    #[test]
    fn structurally_ready_returns_true_for_complete_recommendation() {
        let rec = make_recommendation();
        assert!(rec.is_structurally_ready_for_publication());
    }

    #[test]
    fn structurally_ready_returns_false_without_citations() {
        let mut rec = make_recommendation();
        rec.citations = vec![];
        assert!(!rec.is_structurally_ready_for_publication());
    }

    #[test]
    fn structurally_ready_returns_false_without_requirements() {
        let mut rec = make_recommendation();
        rec.requirement_refs = vec![];
        assert!(!rec.is_structurally_ready_for_publication());
    }

    #[test]
    fn structurally_ready_returns_false_with_incomplete_challenge() {
        let mut rec = make_recommendation();
        rec.spec_challenge.assumptions = ChallengeCategory::new(vec![]); // silent-empty
        assert!(!rec.is_structurally_ready_for_publication());
    }

    #[test]
    fn distinct_citation_count_dedupes_repeated_artifact_ids() {
        let mut rec = make_recommendation();
        rec.citations = vec![
            artifact("alpha"),
            artifact("beta"),
            artifact("alpha"), // duplicate
        ];
        assert_eq!(rec.distinct_citation_count(), 2);
    }

    #[test]
    fn distinct_citation_count_zero_for_empty() {
        let mut rec = make_recommendation();
        rec.citations = vec![];
        assert_eq!(rec.distinct_citation_count(), 0);
    }

    #[test]
    fn draft_request_roundtrips_through_json() {
        let original = BaDraftRequest {
            brief: "make the platform fast".to_string(),
            stakeholder_audience: StakeholderAudience::Customer,
            constraints: vec!["no PII".to_string(), "cite ≥ 2 sources".to_string()],
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: BaDraftRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn recommendation_roundtrips_through_json() {
        let original = make_recommendation();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: BaRecommendation = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RecommendationId>();
        assert_send_sync::<StakeholderAudience>();
        assert_send_sync::<BaDraftRequest>();
        assert_send_sync::<BaRecommendation>();
    }
}
