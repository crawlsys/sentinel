//! A13 — Spec-challenge domain types.
//!
//! Per `docs/a13-spec-challenge.md` §2. The acting agent emits a
//! [`SpecChallenge`] before any Irreversible+ action; the
//! `spec_challenge_gate` hook (future Phase 3) reads it and blocks
//! when the completeness rules below fail.
//!
//! ## What lives here vs. what doesn't
//!
//! - **Here (Phase 1)**: data shapes for the 5 required categories
//!   plus a deterministic [`SpecChallenge::completeness_findings`]
//!   that names every silent-empty category.
//! - **Phase 2 (ports)**: `SpecChallengeScorerPort` (semantic
//!   scoring for Catastrophic-class challenges) plus
//!   `SpecChallengeStorePort` (persistence).
//! - **Phase 3 (hook)**: the actual `PreToolUse` gate. The hook
//!   layer decides whether a challenge is **required** based on
//!   the upcoming work's [`ReversibilityClass`] — this module only
//!   defines what a challenge *is*.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::reversibility::ReversibilityClass;

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// Opaque identifier for the unit of work the challenge is gating.
/// Threaded through the hook's `extra` payload by the orchestrator
/// so multiple challenges in flight stay distinguishable.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkId(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkIdError {
    Empty,
}

impl std::fmt::Display for WorkIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "work_id cannot be empty"),
        }
    }
}

impl std::error::Error for WorkIdError {}

impl WorkId {
    /// Construct, rejecting empty / whitespace-only inputs.
    pub fn new(s: impl Into<String>) -> Result<Self, WorkIdError> {
        let trimmed = s.into().trim().to_string();
        if trimmed.is_empty() {
            return Err(WorkIdError::Empty);
        }
        Ok(Self(trimmed))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Pointer to the spec being challenged.
///
/// The `hash` lets reviewers confirm the agent challenged the spec
/// they think it did; if the spec changes after the challenge is
/// recorded, the hash mismatch surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecReference {
    /// Canonical hash over the spec content (algorithm operator-
    /// chosen; sentinel does not interpret the bytes).
    pub hash: String,
    /// Human-readable source identifier ("PR #123", "issue FPCRM-42",
    /// the literal user-prompt text, etc.).
    pub source: String,
}

// ---------------------------------------------------------------------------
// Category items
// ---------------------------------------------------------------------------

/// How confident the agent is in an assumption. Drives the
/// completeness threshold — `Low` confidence assumptions backing
/// Catastrophic work get scrutinized harder at the scoring phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssumptionConfidence {
    Low,
    Medium,
    High,
}

/// An assumption the agent is operating under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assumption {
    pub statement: String,
    pub confidence: AssumptionConfidence,
    /// What blast-radius class the work has if this assumption is
    /// wrong. Lets the gate weight "Low-confidence assumptions that
    /// would cause Catastrophic blast" higher.
    pub blast_if_wrong: ReversibilityClass,
}

/// How a spec gap was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GapResolution {
    /// Operator clarified the gap before the agent proceeded.
    OperatorClarified,
    /// Agent inferred from prior context; the [`SpecGap::inference_source`]
    /// must name where the inference came from.
    InferredFromContext,
    /// Agent applied a documented default for unspecified behavior.
    DefaultApplied,
}

/// A part of the spec that didn't say something the agent needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecGap {
    pub topic: String,
    pub how_resolved: GapResolution,
    /// Required when `how_resolved == InferredFromContext` —
    /// the [`SpecChallenge::completeness_findings`] check rejects
    /// the gap as malformed when the source is absent for an
    /// inferred resolution.
    pub inference_source: Option<String>,
}

/// A spec passage with multiple plausible readings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ambiguity {
    pub spec_excerpt: String,
    /// Every plausible interpretation the agent surfaced. At least
    /// two are expected for a real ambiguity; one-interpretation
    /// "ambiguities" are flagged as malformed.
    pub interpretations: Vec<String>,
    pub chosen: String,
    pub rationale: String,
}

/// An alternative approach the agent considered before settling on
/// the chosen action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alternative {
    pub description: String,
    pub why_rejected: String,
}

/// A constraint the chosen approach knowingly fails to satisfy.
/// Surfacing these prevents the "we shipped without the constraint
/// because no one stated it" failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsatisfiedConstraint {
    pub constraint: String,
    pub why_not_satisfiable: String,
    pub workaround: Option<String>,
}

// ---------------------------------------------------------------------------
// ChallengeCategory<T>
// ---------------------------------------------------------------------------

/// One of the five challenge categories.
///
/// An empty `items` list is only valid when paired with an
/// `explicit_assertion_of_none` — silent empties are the diagnostic
/// shape of an agent operating on confident misreading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeCategory<T> {
    pub items: Vec<T>,
    /// Required when `items` is empty. Carries the agent's reason
    /// for asserting nothing in this category (e.g., "spec is fully
    /// specified; no gaps").
    pub explicit_assertion_of_none: Option<String>,
}

impl<T> ChallengeCategory<T> {
    /// Construct from items; an empty `items` requires
    /// `explicit_assertion_of_none` to be `Some(_)` to pass
    /// completeness — that contract is enforced by
    /// [`SpecChallenge::completeness_findings`], not at construction.
    #[must_use]
    pub const fn new(items: Vec<T>) -> Self {
        Self {
            items,
            explicit_assertion_of_none: None,
        }
    }

    /// Construct an explicit-none category with a reason.
    #[must_use]
    pub fn none(reason: impl Into<String>) -> Self {
        Self {
            items: Vec::new(),
            explicit_assertion_of_none: Some(reason.into()),
        }
    }

    /// True when the category is "filled" — either has items or has
    /// an explicit-none assertion.
    #[must_use]
    pub fn is_filled(&self) -> bool {
        !self.items.is_empty() || self.explicit_assertion_of_none.is_some()
    }
}

// ---------------------------------------------------------------------------
// SpecChallenge + CompletenessFinding
// ---------------------------------------------------------------------------

/// The five challenge categories an agent must address before any
/// Irreversible+ action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecChallenge {
    pub work_id: WorkId,
    /// Free-form identifier for the agent that produced the
    /// challenge. Sentinel doesn't validate this is in the agent
    /// registry — the field exists for audit attribution.
    pub agent_id: String,
    pub challenged_spec: SpecReference,
    pub reversibility_class: ReversibilityClass,
    pub assumptions: ChallengeCategory<Assumption>,
    pub gaps: ChallengeCategory<SpecGap>,
    pub ambiguities: ChallengeCategory<Ambiguity>,
    pub alternatives_considered: ChallengeCategory<Alternative>,
    pub constraints_not_satisfied: ChallengeCategory<UnsatisfiedConstraint>,
    pub created_at: DateTime<Utc>,
}

/// A category in a [`SpecChallenge`] that the deterministic
/// completeness check has a problem with.
///
/// The hook layer turns these into block decisions per
/// [`ReversibilityClass`]; the domain just identifies them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletenessFinding {
    /// Category has no items and no explicit-none assertion. The
    /// most-common failure shape and the one A13 exists to catch.
    SilentEmpty { category: ChallengeCategoryName },
    /// An ambiguity entry has fewer than two interpretations.
    /// One-interpretation "ambiguities" indicate the agent didn't
    /// actually see a fork — surfacing as malformed.
    InsufficientInterpretations {
        excerpt_preview: String,
        count: usize,
    },
    /// A gap resolved via `InferredFromContext` is missing the
    /// `inference_source` — the inference is unaudited.
    InferenceWithoutSource { topic: String },
}

impl CompletenessFinding {
    /// True for every finding — the hook layer collapses every
    /// finding into a block for Irreversible+ challenges; this
    /// method exists for symmetry with BA findings + future
    /// semantic-only findings that might warn-only.
    #[must_use]
    pub const fn is_block(&self) -> bool {
        true
    }
}

/// Names of the five challenge categories. Used in
/// [`CompletenessFinding::SilentEmpty`] so dashboards can render
/// "missing: gaps" without parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChallengeCategoryName {
    Assumptions,
    Gaps,
    Ambiguities,
    AlternativesConsidered,
    ConstraintsNotSatisfied,
}

impl ChallengeCategoryName {
    /// Stable string identifier for serialization + reporting.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Assumptions => "assumptions",
            Self::Gaps => "gaps",
            Self::Ambiguities => "ambiguities",
            Self::AlternativesConsidered => "alternatives_considered",
            Self::ConstraintsNotSatisfied => "constraints_not_satisfied",
        }
    }
}

impl SpecChallenge {
    /// Run the deterministic completeness check across all five
    /// categories. Returns every issue found; an empty `Vec` means
    /// the challenge is structurally complete.
    ///
    /// Semantic quality (do the assumptions actually cover the
    /// risky parts? are the rejections of alternatives substantive?)
    /// is a separate Phase 2 scoring concern; this method is the
    /// deterministic floor below which a challenge can't pass.
    #[must_use]
    pub fn completeness_findings(&self) -> Vec<CompletenessFinding> {
        let mut findings = Vec::new();
        check_filled(
            &self.assumptions,
            ChallengeCategoryName::Assumptions,
            &mut findings,
        );
        check_filled_with(
            &self.gaps,
            ChallengeCategoryName::Gaps,
            &mut findings,
            |item| {
                if matches!(item.how_resolved, GapResolution::InferredFromContext)
                    && item.inference_source.as_deref().unwrap_or("").trim().is_empty()
                {
                    Some(CompletenessFinding::InferenceWithoutSource {
                        topic: item.topic.clone(),
                    })
                } else {
                    None
                }
            },
        );
        check_filled_with(
            &self.ambiguities,
            ChallengeCategoryName::Ambiguities,
            &mut findings,
            |item| {
                if item.interpretations.len() < 2 {
                    Some(CompletenessFinding::InsufficientInterpretations {
                        excerpt_preview: preview(&item.spec_excerpt, 60),
                        count: item.interpretations.len(),
                    })
                } else {
                    None
                }
            },
        );
        check_filled(
            &self.alternatives_considered,
            ChallengeCategoryName::AlternativesConsidered,
            &mut findings,
        );
        check_filled(
            &self.constraints_not_satisfied,
            ChallengeCategoryName::ConstraintsNotSatisfied,
            &mut findings,
        );
        findings
    }

    /// True when the deterministic completeness check finds no
    /// issues. Convenience wrapper for the hook layer's most-common
    /// shape ("if !complete -> block").
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.completeness_findings().is_empty()
    }
}

fn check_filled<T>(
    cat: &ChallengeCategory<T>,
    name: ChallengeCategoryName,
    out: &mut Vec<CompletenessFinding>,
) {
    if !cat.is_filled() {
        out.push(CompletenessFinding::SilentEmpty { category: name });
    }
}

fn check_filled_with<T, F>(
    cat: &ChallengeCategory<T>,
    name: ChallengeCategoryName,
    out: &mut Vec<CompletenessFinding>,
    per_item: F,
) where
    F: Fn(&T) -> Option<CompletenessFinding>,
{
    if !cat.is_filled() {
        out.push(CompletenessFinding::SilentEmpty { category: name });
        return;
    }
    for item in &cat.items {
        if let Some(finding) = per_item(item) {
            out.push(finding);
        }
    }
}

fn preview(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn filled_challenge() -> SpecChallenge {
        SpecChallenge {
            work_id: WorkId::new("w1").unwrap(),
            agent_id: "claude-opus-4-7".to_string(),
            challenged_spec: SpecReference {
                hash: "abc123".to_string(),
                source: "issue FPCRM-42".to_string(),
            },
            reversibility_class: ReversibilityClass::Irreversible,
            assumptions: ChallengeCategory::new(vec![Assumption {
                statement: "operator wants Postgres".to_string(),
                confidence: AssumptionConfidence::High,
                blast_if_wrong: ReversibilityClass::Irreversible,
            }]),
            gaps: ChallengeCategory::new(vec![SpecGap {
                topic: "deployment target".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            }]),
            ambiguities: ChallengeCategory::new(vec![Ambiguity {
                spec_excerpt: "make it fast".to_string(),
                interpretations: vec![
                    "p99 latency < 100ms".to_string(),
                    "throughput > 1k qps".to_string(),
                ],
                chosen: "p99 latency < 100ms".to_string(),
                rationale: "earlier context emphasized user-visible delay".to_string(),
            }]),
            alternatives_considered: ChallengeCategory::new(vec![Alternative {
                description: "use Redis for the queue".to_string(),
                why_rejected: "extra ops surface; durability story weaker".to_string(),
            }]),
            constraints_not_satisfied: ChallengeCategory::none(
                "all stated constraints satisfied".to_string(),
            ),
            created_at: ts(),
        }
    }

    #[test]
    fn work_id_rejects_empty() {
        assert!(WorkId::new("").is_err());
        assert!(WorkId::new("   ").is_err());
    }

    #[test]
    fn work_id_trims_whitespace() {
        let id = WorkId::new("  w1  ").unwrap();
        assert_eq!(id.as_str(), "w1");
    }

    #[test]
    fn challenge_category_is_filled_when_items_present() {
        let cat: ChallengeCategory<Assumption> = ChallengeCategory::new(vec![Assumption {
            statement: "x".to_string(),
            confidence: AssumptionConfidence::High,
            blast_if_wrong: ReversibilityClass::Irreversible,
        }]);
        assert!(cat.is_filled());
    }

    #[test]
    fn challenge_category_is_filled_when_explicit_none_set() {
        let cat: ChallengeCategory<Assumption> = ChallengeCategory::none("none here");
        assert!(cat.is_filled());
    }

    #[test]
    fn challenge_category_is_not_filled_when_silently_empty() {
        let cat: ChallengeCategory<Assumption> = ChallengeCategory::new(vec![]);
        assert!(!cat.is_filled());
    }

    #[test]
    fn fully_filled_challenge_has_no_completeness_findings() {
        let challenge = filled_challenge();
        let findings = challenge.completeness_findings();
        assert!(findings.is_empty(), "got {findings:?}");
        assert!(challenge.is_complete());
    }

    #[test]
    fn silent_empty_assumptions_surface_finding() {
        let mut challenge = filled_challenge();
        challenge.assumptions = ChallengeCategory::new(vec![]);
        let findings = challenge.completeness_findings();
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0],
            CompletenessFinding::SilentEmpty {
                category: ChallengeCategoryName::Assumptions
            }
        ));
        assert!(!challenge.is_complete());
    }

    #[test]
    fn explicit_none_assumptions_is_complete() {
        let mut challenge = filled_challenge();
        challenge.assumptions = ChallengeCategory::none("no assumptions made");
        assert!(challenge.is_complete());
    }

    #[test]
    fn all_five_categories_silent_empty_yields_five_findings() {
        let mut challenge = filled_challenge();
        challenge.assumptions = ChallengeCategory::new(vec![]);
        challenge.gaps = ChallengeCategory::new(vec![]);
        challenge.ambiguities = ChallengeCategory::new(vec![]);
        challenge.alternatives_considered = ChallengeCategory::new(vec![]);
        challenge.constraints_not_satisfied = ChallengeCategory::new(vec![]);
        let findings = challenge.completeness_findings();
        assert_eq!(findings.len(), 5);
        for f in &findings {
            assert!(matches!(f, CompletenessFinding::SilentEmpty { .. }));
            assert!(f.is_block());
        }
    }

    #[test]
    fn ambiguity_with_single_interpretation_surfaces_finding() {
        let mut challenge = filled_challenge();
        challenge.ambiguities = ChallengeCategory::new(vec![Ambiguity {
            spec_excerpt: "ship it".to_string(),
            interpretations: vec!["only one".to_string()],
            chosen: "only one".to_string(),
            rationale: "obvious".to_string(),
        }]);
        let findings = challenge.completeness_findings();
        let bad = findings
            .iter()
            .find(|f| matches!(f, CompletenessFinding::InsufficientInterpretations { .. }))
            .expect("should find insufficient-interpretations");
        if let CompletenessFinding::InsufficientInterpretations { count, .. } = bad {
            assert_eq!(*count, 1);
        }
    }

    #[test]
    fn gap_inferred_without_source_surfaces_finding() {
        let mut challenge = filled_challenge();
        challenge.gaps = ChallengeCategory::new(vec![SpecGap {
            topic: "auth method".to_string(),
            how_resolved: GapResolution::InferredFromContext,
            inference_source: None,
        }]);
        let findings = challenge.completeness_findings();
        let bad = findings
            .iter()
            .find(|f| matches!(f, CompletenessFinding::InferenceWithoutSource { .. }))
            .expect("should find inference-without-source");
        if let CompletenessFinding::InferenceWithoutSource { topic } = bad {
            assert_eq!(topic, "auth method");
        }
    }

    #[test]
    fn gap_inferred_with_empty_string_source_surfaces_finding() {
        // Empty string is whitespace-equivalent to None for source purposes.
        let mut challenge = filled_challenge();
        challenge.gaps = ChallengeCategory::new(vec![SpecGap {
            topic: "x".to_string(),
            how_resolved: GapResolution::InferredFromContext,
            inference_source: Some("   ".to_string()),
        }]);
        let findings = challenge.completeness_findings();
        assert!(findings
            .iter()
            .any(|f| matches!(f, CompletenessFinding::InferenceWithoutSource { .. })));
    }

    #[test]
    fn gap_resolved_otherwise_does_not_need_source() {
        let mut challenge = filled_challenge();
        challenge.gaps = ChallengeCategory::new(vec![
            SpecGap {
                topic: "x".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            },
            SpecGap {
                topic: "y".to_string(),
                how_resolved: GapResolution::DefaultApplied,
                inference_source: None,
            },
        ]);
        let findings = challenge.completeness_findings();
        assert!(findings.is_empty(), "got {findings:?}");
    }

    #[test]
    fn challenge_category_name_keys_are_snake_case() {
        for (name, expected) in [
            (ChallengeCategoryName::Assumptions, "assumptions"),
            (ChallengeCategoryName::Gaps, "gaps"),
            (ChallengeCategoryName::Ambiguities, "ambiguities"),
            (
                ChallengeCategoryName::AlternativesConsidered,
                "alternatives_considered",
            ),
            (
                ChallengeCategoryName::ConstraintsNotSatisfied,
                "constraints_not_satisfied",
            ),
        ] {
            assert_eq!(name.key(), expected);
        }
    }

    #[test]
    fn challenge_roundtrips_through_json() {
        let original = filled_challenge();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: SpecChallenge = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WorkId>();
        assert_send_sync::<SpecChallenge>();
        assert_send_sync::<CompletenessFinding>();
        assert_send_sync::<ChallengeCategoryName>();
    }
}
