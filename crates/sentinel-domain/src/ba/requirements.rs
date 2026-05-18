//! BA3 — Requirements traceability domain types.
//!
//! Per `docs/ba1-ba3-sentinel-enforcement.md` §2.3 + §9. Defines the
//! wire-format type consul ADR-017 specifies for requirement
//! references (`RequirementRef`) plus the sentinel-side check +
//! finding enums the `requirements_traceability_gate` hook (future
//! phase) emits.
//!
//! Companion to [`crate::ba::provenance`]: BA1 makes citations
//! structural, BA3 makes recommendation→requirement traceability
//! structural. Together they define the structural-enforcement
//! substrate for the BA-vertical S-tier disciplines.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Wire-format type (consul ADR-017).
// ---------------------------------------------------------------------------

/// Requirement-matrix reference embedded in BA-orchestrator outputs.
///
/// Carried by `RelayInstruction` and `InstructionResult` per consul
/// ADR-017. Sentinel's `requirements_traceability_gate` hook (future
/// phase) reads `Vec<RequirementRef>` from the output and validates
/// each against the orchestrator's published requirement matrix.
///
/// The four-tuple `(orchestration_id, matrix_row_id, content_hash,
/// statement)` uniquely identifies a stakeholder need at a specific
/// version. `statement` is the human-readable requirement text;
/// `content_hash` lets sentinel detect when a recommendation cites a
/// stale row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequirementRef {
    /// Stable identifier for the requirement matrix this row lives
    /// in (typically the BA-orchestrator's case identifier).
    pub orchestration_id: String,
    /// Stable identifier for the row within the matrix.
    pub matrix_row_id: String,
    /// Content hash of the requirement statement at citation time.
    /// Mismatch with the live matrix → stale-reference warning.
    pub content_hash: String,
    /// Human-readable requirement text (e.g., "Stakeholder requires
    /// month-over-month churn under 2%"). Cached locally so the
    /// gate doesn't need a round-trip to render operator messages.
    pub statement: String,
}

// ---------------------------------------------------------------------------
// Check + finding types.
// ---------------------------------------------------------------------------

/// Result of validating a BA-orchestrator output's `requirement_refs`
/// list against the requirement matrix.
///
/// Aggregate over the full set — a recommendation may cite multiple
/// requirements and a single missing one is enough to flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementCheck {
    /// The references the output claimed.
    pub references: Vec<RequirementRef>,
    pub findings: Vec<RequirementFinding>,
}

impl RequirementCheck {
    /// Construct with the cited references and no findings (pass).
    #[must_use]
    pub const fn passing(references: Vec<RequirementRef>) -> Self {
        Self {
            references,
            findings: Vec::new(),
        }
    }

    /// Builder-style: attach a finding.
    #[must_use]
    pub fn with_finding(mut self, finding: RequirementFinding) -> Self {
        self.findings.push(finding);
        self
    }

    /// Returns `true` iff at least one Block-class finding is
    /// present.
    #[must_use]
    pub fn has_block(&self) -> bool {
        self.findings.iter().any(RequirementFinding::is_block)
    }
}

/// Specific requirements-traceability failure modes per spec §2.3
/// decision tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequirementFinding {
    /// Cited matrix row doesn't exist in the orchestrator's matrix.
    /// **Block-class** — recommendation traces to a phantom requirement.
    Existence {
        orchestration_id: String,
        matrix_row_id: String,
    },

    /// Matrix lookup found the row but its `content_hash` differs
    /// from the citation. Warn — the requirement was edited after
    /// the recommendation was authored; operator should confirm the
    /// recommendation still answers the current requirement.
    Hash {
        orchestration_id: String,
        matrix_row_id: String,
        cited_hash: String,
        actual_hash: String,
    },

    /// Output classified as a recommendation but ships with an
    /// **empty** `requirement_refs` list. **Block-class** — this is
    /// the structural violation BA3 exists to prevent.
    Coverage { recommendation_summary: String },

    /// Matrix endpoint unreachable; gate is running against the
    /// `last_known_good` snapshot. Warn — not a content failure but
    /// a freshness concern the operator should see.
    MatrixStaleness { snapshot_age_seconds: u64 },
}

impl RequirementFinding {
    /// Returns `true` iff this finding blocks the BA-orchestrator
    /// output.
    #[must_use]
    pub const fn is_block(&self) -> bool {
        matches!(self, Self::Existence { .. } | Self::Coverage { .. })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ref() -> RequirementRef {
        RequirementRef {
            orchestration_id: "case-2026-Q2-pricing".into(),
            matrix_row_id: "R-001".into(),
            content_hash: "hash-v1".into(),
            statement: "Stakeholder requires month-over-month churn under 2%.".into(),
        }
    }

    #[test]
    fn passing_check_has_no_findings() {
        let check = RequirementCheck::passing(vec![fixture_ref()]);
        assert!(!check.has_block());
        assert!(check.findings.is_empty());
    }

    #[test]
    fn existence_finding_is_block() {
        let f = RequirementFinding::Existence {
            orchestration_id: "x".into(),
            matrix_row_id: "y".into(),
        };
        assert!(f.is_block());
    }

    #[test]
    fn coverage_finding_is_block() {
        let f = RequirementFinding::Coverage {
            recommendation_summary: "Raise prices by 8%".into(),
        };
        assert!(f.is_block(), "BA3 coverage failure is the structural-violation case");
    }

    #[test]
    fn hash_mismatch_is_warn_not_block() {
        let f = RequirementFinding::Hash {
            orchestration_id: "x".into(),
            matrix_row_id: "y".into(),
            cited_hash: "old".into(),
            actual_hash: "new".into(),
        };
        assert!(!f.is_block());
    }

    #[test]
    fn matrix_staleness_is_warn() {
        let f = RequirementFinding::MatrixStaleness {
            snapshot_age_seconds: 3600,
        };
        assert!(!f.is_block());
    }

    #[test]
    fn check_has_block_when_any_finding_blocks() {
        let check = RequirementCheck::passing(vec![fixture_ref()])
            .with_finding(RequirementFinding::Hash {
                orchestration_id: "x".into(),
                matrix_row_id: "y".into(),
                cited_hash: "old".into(),
                actual_hash: "new".into(),
            })
            .with_finding(RequirementFinding::Coverage {
                recommendation_summary: "Raise prices".into(),
            });
        assert!(check.has_block());
    }

    #[test]
    fn requirement_ref_roundtrips_through_json() {
        let original = fixture_ref();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: RequirementRef = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RequirementRef>();
        assert_send_sync::<RequirementCheck>();
        assert_send_sync::<RequirementFinding>();
    }
}
