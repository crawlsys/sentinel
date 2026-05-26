//! A12 — Eval case domain types.
//!
//! Per `docs/a12-external-benchmarks.md` §3.2. An `EvalCase` is one
//! benchmark unit: a stakeholder brief, the source materials, an
//! optional gold artifact + outcomes, and the scoring rubric.
//! Corpora are collections of cases stored at
//! `~/.claude/sentinel/eval/ba-corpus/cases/{case_id}.json` (storage
//! adapter ships in a future phase).

use std::fmt;

use serde::{Deserialize, Serialize};

use super::rubric::ScoringRubric;

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// Stable identifier for an eval case. Lives in
/// `cases/{case_id}.json` on disk.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EvalCaseId(String);

impl EvalCaseId {
    /// Construct from a string. Trims; empty after-trim is
    /// rejected — running an eval against an unnamed case is
    /// always a bug.
    pub fn new(s: impl Into<String>) -> Result<Self, EvalIdError> {
        let trimmed = s.into().trim().to_string();
        if trimmed.is_empty() {
            return Err(EvalIdError::Empty("EvalCaseId"));
        }
        Ok(Self(trimmed))
    }

    /// Borrow as `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EvalCaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identifier for an eval run. Lives in
/// `runs/{run_id}.jsonl` on disk.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EvalRunId(String);

impl EvalRunId {
    /// Construct from a string. Empty after-trim is rejected.
    pub fn new(s: impl Into<String>) -> Result<Self, EvalIdError> {
        let trimmed = s.into().trim().to_string();
        if trimmed.is_empty() {
            return Err(EvalIdError::Empty("EvalRunId"));
        }
        Ok(Self(trimmed))
    }

    /// Borrow as `&str`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EvalRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Construction errors for eval IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalIdError {
    /// Empty / whitespace-only string passed to `new`.
    Empty(&'static str),
}

impl fmt::Display for EvalIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(name) => write!(f, "{name} must not be empty"),
        }
    }
}

impl std::error::Error for EvalIdError {}

// ---------------------------------------------------------------------------
// Source corpora
// ---------------------------------------------------------------------------

/// Where the case's materials came from.
///
/// Per spec §3.2 the three pools are explicit so reporting can break
/// down scores by source type (public corpus vs partner-contributed
/// vs synthetic — wildly different signal qualities).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceCorpus {
    /// Public consulting deliverables (`McKinsey` / Bain / BCG public
    /// reports, academic case studies, public think-tank briefs).
    /// High availability, lower-quality outcome data.
    Public { url: String, license: String },
    /// Partner-contributed real BA work (with consent + redaction
    /// per `redaction_level`). Highest signal because outcomes are
    /// real; rare because of confidentiality. The `partner` field
    /// is the operator's reference; sentinel doesn't surface it
    /// without explicit operator action.
    PartnerContributed {
        partner: String,
        redaction_level: RedactionLevel,
    },
    /// Synthetic generated case used to fill capability gaps (e.g.,
    /// "we have lots of strategy briefs but few pricing analyses").
    /// `generation_method` documents how it was generated.
    /// **Outcome-scoring is invalid** on synthetic cases — their
    /// gold outcomes are fabricated.
    SyntheticGenerated { generation_method: String },
}

/// How aggressively the source materials were redacted before
/// inclusion in the corpus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RedactionLevel {
    /// No redaction — public domain materials.
    None,
    /// Names + dates anonymized; financials proportionalized.
    Light,
    /// Industry obfuscated; revenue ranges + dates anonymized;
    /// strategic content preserved.
    Heavy,
    /// Only structural patterns retained; specific content
    /// reworded enough to defeat string search.
    Synthetic,
}

// ---------------------------------------------------------------------------
// Gold reference data
// ---------------------------------------------------------------------------

/// The human-authored "good" output for a case.
///
/// What a strong BA would have produced given the stakeholder brief
/// and source materials. Used to score the agent's output against a
/// known-good reference.
///
/// Optional because gold data isn't always available (especially for
/// synthetic cases).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GoldArtifact {
    /// Free-text rendering of the gold output. May include
    /// markdown, citations, etc.
    pub text: String,
    /// Author / source attribution for the gold artifact.
    pub author: String,
    /// Optional content hash for stability across corpus revisions.
    pub content_hash: Option<String>,
}

/// What actually happened in the real world after the stakeholder
/// acted on the BA's recommendation.
///
/// Used to score the outcome-realism axis. Only available for
/// partner-contributed cases that include follow-up outcome data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldOutcomes {
    /// Free-text outcome summary (e.g., "recommendation adopted;
    /// quarterly churn dropped 1.7% within 6 months").
    pub summary: String,
    /// Time window over which the outcome was observed.
    pub observation_window_days: u32,
    /// Operator-supplied confidence the outcome can be attributed
    /// to the recommendation specifically (vs. confounders). 0.0-1.0.
    pub attribution_confidence: f32,
}

// ---------------------------------------------------------------------------
// Case provenance
// ---------------------------------------------------------------------------

/// Sourcing / consent / license metadata for a case. Required —
/// every case must declare its provenance so reporting can disclose
/// (or operators can audit) the corpus composition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaseProvenance {
    /// Who contributed this case (operator name / partner name /
    /// "public-domain").
    pub contributor: String,
    /// License string verbatim from the source. For public
    /// materials this matches the published license; for partner
    /// contributions it's the operator's negotiated terms.
    pub license: String,
    /// Whether the case is part of the held-back private test
    /// split. The storage adapter (future phase) enforces that
    /// `is_private_test == true` cases are never exposed during
    /// prompt iteration — only during honest measurement runs.
    pub is_private_test: bool,
}

// ---------------------------------------------------------------------------
// EvalCase
// ---------------------------------------------------------------------------

/// One benchmark unit. Per spec §3.2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCase {
    pub case_id: EvalCaseId,
    /// The original request, verbatim (possibly redacted).
    pub stakeholder_brief: String,
    pub source_corpus: SourceCorpus,
    pub gold_artifact: Option<GoldArtifact>,
    pub gold_outcomes: Option<GoldOutcomes>,
    pub scoring_rubric: ScoringRubric,
    pub provenance: CaseProvenance,
}

impl EvalCase {
    /// Returns `true` iff `outcome_realism` axis scoring is valid
    /// for this case — i.e., gold outcomes are present AND the
    /// source isn't synthetic (synthetic outcomes aren't real and
    /// should not be scored against).
    #[must_use]
    pub const fn outcome_scoring_valid(&self) -> bool {
        if self.gold_outcomes.is_none() {
            return false;
        }
        !matches!(self.source_corpus, SourceCorpus::SyntheticGenerated { .. })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::rubric::ScoringRubric;

    fn fixture_case() -> EvalCase {
        EvalCase {
            case_id: EvalCaseId::new("strategy-case-001").unwrap(),
            stakeholder_brief: "Recommend pricing strategy for SaaS expansion to EU".into(),
            source_corpus: SourceCorpus::Public {
                url: "https://example.com/cases/001".into(),
                license: "CC-BY-4.0".into(),
            },
            gold_artifact: Some(GoldArtifact {
                text: "Differentiated tiered pricing aligned to EU value perception".into(),
                author: "case-curator".into(),
                content_hash: Some("hash-1".into()),
            }),
            gold_outcomes: None,
            scoring_rubric: ScoringRubric::ba_default(),
            provenance: CaseProvenance {
                contributor: "public-domain".into(),
                license: "CC-BY-4.0".into(),
                is_private_test: false,
            },
        }
    }

    #[test]
    fn eval_case_id_rejects_empty() {
        assert_eq!(EvalCaseId::new(""), Err(EvalIdError::Empty("EvalCaseId")));
        assert_eq!(
            EvalCaseId::new("   "),
            Err(EvalIdError::Empty("EvalCaseId"))
        );
    }

    #[test]
    fn eval_case_id_trims_whitespace() {
        let id = EvalCaseId::new("  case-1  ").unwrap();
        assert_eq!(id.as_str(), "case-1");
    }

    #[test]
    fn eval_run_id_rejects_empty() {
        assert_eq!(EvalRunId::new(""), Err(EvalIdError::Empty("EvalRunId")));
    }

    #[test]
    fn outcome_scoring_invalid_when_outcomes_missing() {
        let case = fixture_case();
        assert!(!case.outcome_scoring_valid());
    }

    #[test]
    fn outcome_scoring_invalid_on_synthetic_even_with_gold_outcomes() {
        // Synthetic source + gold_outcomes present → still invalid
        // (the spec is explicit: synthetic outcomes aren't real).
        let mut case = fixture_case();
        case.source_corpus = SourceCorpus::SyntheticGenerated {
            generation_method: "llm-curated".into(),
        };
        case.gold_outcomes = Some(GoldOutcomes {
            summary: "fabricated".into(),
            observation_window_days: 90,
            attribution_confidence: 0.8,
        });
        assert!(!case.outcome_scoring_valid());
    }

    #[test]
    fn outcome_scoring_valid_for_partner_with_outcomes() {
        let mut case = fixture_case();
        case.source_corpus = SourceCorpus::PartnerContributed {
            partner: "redacted-partner".into(),
            redaction_level: RedactionLevel::Heavy,
        };
        case.gold_outcomes = Some(GoldOutcomes {
            summary: "Recommendation adopted; churn dropped 1.7%".into(),
            observation_window_days: 180,
            attribution_confidence: 0.7,
        });
        assert!(case.outcome_scoring_valid());
    }

    #[test]
    fn eval_case_roundtrips_through_json() {
        let original = fixture_case();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: EvalCase = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EvalCase>();
        assert_send_sync::<EvalCaseId>();
        assert_send_sync::<EvalRunId>();
        assert_send_sync::<SourceCorpus>();
        assert_send_sync::<RedactionLevel>();
        assert_send_sync::<GoldArtifact>();
        assert_send_sync::<GoldOutcomes>();
        assert_send_sync::<CaseProvenance>();
    }
}
