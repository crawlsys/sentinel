//! BA1 — Citation provenance domain types.
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §2.2 + §9. Defines the
//! wire-format types consul ADR-017 specifies for artifact citations
//! (`ArtifactReference`, `ProvenanceClass`) plus the sentinel-side
//! check + finding enums the `provenance_validate` hook (future
//! phase) emits.
//!
//! No business logic — that lives behind
//! `crate::ports::ProvenancePort` (future phase) which the
//! application-layer hook consults.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Wire-format types (consul ADR-017).
// ---------------------------------------------------------------------------

/// Coarse classification of where a cited artifact came from.
///
/// The `provenance_validate` hook (future phase) checks that the
/// claimed class matches what the connector actually emitted —
/// labeling an `Inference` as `SystemOfRecord` is a structural
/// violation BA1 is designed to catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProvenanceClass {
    /// Authoritative system-of-record (Linear ticket, GitHub commit,
    /// Confluence page version-N, signed PDF, audited financial
    /// statement). The strongest provenance — sentinel treats these
    /// as ground truth subject to freshness checks.
    SystemOfRecord,
    /// Synthesized output from a domain expert (consultant memo,
    /// internal analysis, expert-curated dashboard). Strong but not
    /// authoritative — context for the synthesis matters.
    ExpertSynthesis,
    /// Model inference (LLM summary, classifier output, derived
    /// insight). Useful but must be traceable to a more
    /// authoritative class for any S-tier claim.
    Inference,
    /// Unverified third-party reference (URL, hearsay, "operator
    /// mentioned"). Acceptable as supporting evidence; never as the
    /// sole citation for a recommendation.
    Unverified,
}

/// Citation reference embedded in BA-orchestrator outputs.
///
/// Carried by `RelayInstruction` and `InstructionResult` per consul
/// ADR-017. Sentinel's `provenance_validate` hook (future phase)
/// reads `Vec<ArtifactReference>` from the output and validates each
/// against the connector audit chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactReference {
    /// Stable identifier from the upstream connector (Linear issue
    /// ID, Confluence page ID, etc.).
    pub artifact_id: String,
    /// Content hash at retrieval time. Lets sentinel detect stale
    /// citations (the connector's current hash differs).
    pub content_hash: String,
    /// Class the citing agent claims for this artifact. Validated
    /// against the class the connector reported.
    pub provenance_class: ProvenanceClass,
    /// When the connector retrieval happened. Used by the
    /// freshness check + the within-session check.
    pub retrieved_at: DateTime<Utc>,
}

/// Single retrieval entry in sentinel's connector-audit chain.
///
/// Emitted by the `audit_extract` hook (future phase) on every
/// `PostToolUse` for documented MCP connectors. The
/// `provenance_validate` hook reads these records via a
/// `ProvenancePort` lookup keyed by `artifact_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RetrievalRecord {
    /// Same identifier the citation will use.
    pub artifact_id: String,
    /// MCP connector that retrieved this artifact (e.g.
    /// `mcp__linear__get_issue`).
    pub connector_name: String,
    /// Content hash at retrieval time. Compared against the
    /// citation's claimed hash for freshness validation.
    pub content_hash: String,
    /// Class the connector reported (NOT the agent's claim — that's
    /// the cross-check).
    pub provenance_class: ProvenanceClass,
    /// Session this retrieval occurred in. Within-session check
    /// matches against this; cross-session lookback (default 24h)
    /// is operator-configurable.
    pub session_id: String,
    /// When the retrieval happened.
    pub retrieved_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Check + finding types.
// ---------------------------------------------------------------------------

/// Result of validating one `ArtifactReference` against the audit
/// chain.
///
/// Carries the citation and the per-check verdicts so the
/// `provenance_validate` hook can render a precise operator-facing
/// message (which citation failed, and why).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceCheck {
    pub citation: ArtifactReference,
    pub findings: Vec<ProvenanceFinding>,
}

impl ProvenanceCheck {
    /// Construct an empty check (no findings — citation passes all
    /// rules). Builder pattern with `with_finding`.
    #[must_use]
    pub const fn passing(citation: ArtifactReference) -> Self {
        Self {
            citation,
            findings: Vec::new(),
        }
    }

    /// Builder-style: attach a finding (a specific failure or
    /// warning).
    #[must_use]
    pub fn with_finding(mut self, finding: ProvenanceFinding) -> Self {
        self.findings.push(finding);
        self
    }

    /// Returns `true` iff at least one Block-class finding is
    /// present. The hook denies the call when any check has a Block
    /// finding.
    #[must_use]
    pub fn has_block(&self) -> bool {
        self.findings.iter().any(ProvenanceFinding::is_block)
    }
}

/// Specific provenance failure modes per spec §2.2 decision tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProvenanceFinding {
    /// No matching `RetrievalRecord` for this `artifact_id` in the
    /// audit chain. **Block-class** — citation is unverifiable.
    Existence { artifact_id: String },

    /// Audit-chain `content_hash` differs from the citation's
    /// `content_hash`. Warn for routine outputs; block for
    /// catastrophic-class outputs (operator-configurable).
    Freshness {
        artifact_id: String,
        cited_hash: String,
        actual_hash: String,
        is_blocking: bool,
    },

    /// Citation's `provenance_class` differs from what the connector
    /// reported. Warn — surfaces possible mis-classification (e.g.,
    /// labeling an `Inference` as `SystemOfRecord`).
    ProvenanceClass {
        artifact_id: String,
        cited_class: ProvenanceClass,
        actual_class: ProvenanceClass,
    },

    /// Connector retrieval was older than the configured lookback
    /// window (default 24h; tighter for catastrophic). Warn or
    /// block per `is_blocking`.
    WithinSession {
        artifact_id: String,
        retrieved_at: DateTime<Utc>,
        cutoff: DateTime<Utc>,
        is_blocking: bool,
    },
}

impl ProvenanceFinding {
    /// Returns `true` iff this finding blocks the BA-orchestrator
    /// output. The hook denies on any Block; warns on the rest.
    #[must_use]
    pub const fn is_block(&self) -> bool {
        match self {
            Self::Existence { .. } => true,
            Self::Freshness { is_blocking, .. } | Self::WithinSession { is_blocking, .. } => {
                *is_blocking
            }
            Self::ProvenanceClass { .. } => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_citation() -> ArtifactReference {
        ArtifactReference {
            artifact_id: "FIR-123".into(),
            content_hash: "abcd1234".into(),
            provenance_class: ProvenanceClass::SystemOfRecord,
            retrieved_at: Utc::now(),
        }
    }

    fn fixture_record(now: DateTime<Utc>) -> RetrievalRecord {
        RetrievalRecord {
            artifact_id: "FIR-123".into(),
            connector_name: "mcp__linear__get_issue".into(),
            content_hash: "abcd1234".into(),
            provenance_class: ProvenanceClass::SystemOfRecord,
            session_id: "session-1".into(),
            retrieved_at: now,
        }
    }

    #[test]
    fn passing_check_has_no_findings() {
        let check = ProvenanceCheck::passing(fixture_citation());
        assert!(!check.has_block());
        assert!(check.findings.is_empty());
    }

    #[test]
    fn existence_finding_is_block() {
        let f = ProvenanceFinding::Existence {
            artifact_id: "FIR-123".into(),
        };
        assert!(f.is_block());
    }

    #[test]
    fn freshness_finding_block_per_flag() {
        let blocking = ProvenanceFinding::Freshness {
            artifact_id: "FIR-123".into(),
            cited_hash: "stale".into(),
            actual_hash: "fresh".into(),
            is_blocking: true,
        };
        assert!(blocking.is_block());
        let warn = ProvenanceFinding::Freshness {
            artifact_id: "FIR-123".into(),
            cited_hash: "stale".into(),
            actual_hash: "fresh".into(),
            is_blocking: false,
        };
        assert!(!warn.is_block());
    }

    #[test]
    fn provenance_class_mismatch_is_warn() {
        let f = ProvenanceFinding::ProvenanceClass {
            artifact_id: "FIR-123".into(),
            cited_class: ProvenanceClass::SystemOfRecord,
            actual_class: ProvenanceClass::Inference,
        };
        assert!(
            !f.is_block(),
            "class mismatch is operator-facing warn, not block"
        );
    }

    #[test]
    fn check_has_block_when_any_finding_blocks() {
        let check = ProvenanceCheck::passing(fixture_citation())
            .with_finding(ProvenanceFinding::ProvenanceClass {
                artifact_id: "FIR-123".into(),
                cited_class: ProvenanceClass::SystemOfRecord,
                actual_class: ProvenanceClass::Inference,
            })
            .with_finding(ProvenanceFinding::Existence {
                artifact_id: "FIR-999".into(),
            });
        assert!(check.has_block());
    }

    #[test]
    fn citation_roundtrips_through_json() {
        let original = fixture_citation();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: ArtifactReference = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn record_roundtrips_through_json() {
        let original = fixture_record(Utc::now());
        let json = serde_json::to_string(&original).unwrap();
        let parsed: RetrievalRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ArtifactReference>();
        assert_send_sync::<RetrievalRecord>();
        assert_send_sync::<ProvenanceClass>();
        assert_send_sync::<ProvenanceCheck>();
        assert_send_sync::<ProvenanceFinding>();
    }
}
