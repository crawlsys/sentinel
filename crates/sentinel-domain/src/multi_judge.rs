//! Multi-judge verdict types — Stage B of pluggable judges
//! (#82, follow-up to commit 2089896 which shipped Stage A).
//!
//! Stage A made the `JudgeModel` enum drive OpenRouter dispatch
//! (one tier → one model). Stage B turns "one judge" into "N judges
//! with disagreement detection" so the `critical` and
//! `critical-strict` trust tiers can run multiple model families in
//! parallel and surface disagreements as a chain entry.
//!
//! These types live in `sentinel-domain` so the wire format is
//! locked before the infrastructure-side producer
//! (`rig_judge::evaluate_multi`) and the chain-entry consumer
//! (`ProofEntry::Disagreement`) wire in. Same staging strategy as
//! M3.3's `review.rs` — types first, producers/consumers next.
//!
//! # Trust tiers
//!
//! | Tier | Judges run | Purpose |
//! |------|-----------|---------|
//! | `routine` | 1 (Haiku) | Fast tier — cheap routine phases |
//! | `review` (default) | 1 (Kimi) | Single-judge review path |
//! | `critical` | 2 (Kimi + Sonnet) | Cross-vendor pair |
//! | `critical-strict` | 3 (Kimi + Sonnet + Opus) | Trio for highest-stakes |
//! | `audit-grade` | 1 (Kimi) + provider recorded | Compliance: replay-able provenance |
//!
//! Cross-distribution diversity (Eastern OSS Kimi + Western closed
//! GPT/Opus) is the load-bearing property: 3 closed-frontier models
//! share more failure modes than people admit.
//!
//! # Synthesis rules (worst-case-wins)
//!
//! Mirrors `MultiLensReview` from M3.3 — same
//! "worst-of-N decides the chain" pattern, applied to per-judge
//! `JudgeVerdict` outcomes:
//!
//! 1. **Sufficient when all sufficient**. Any single judge returning
//!    `sufficient: false` flips the overall to `false` —
//!    conservative by design. The orchestrator should never miss a
//!    "I don't think this passed" because two other judges said
//!    yes.
//! 2. **Confidence is the floor (min)**. The worst-case judge's
//!    confidence is the overall confidence. Averaging would let one
//!    high-confidence judge mask another's uncertainty.
//! 3. **Disagreement is detected when verdicts differ**. The
//!    `disagreement` field on `MultiJudgeVerdict` is `true` iff at
//!    least one pair of judges disagrees on `sufficient`. This
//!    drives the `ProofEntry::Disagreement` chain entry that lands
//!    in a follow-up commit.

use serde::{Deserialize, Serialize};

use crate::judge::{JudgeModel, JudgeVerdict};

/// Trust tier governing how many judges run and which models.
/// Step configs declare this; runtime maps it to the model list via
/// [`JudgeTrustTier::judge_models`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JudgeTrustTier {
    /// Single Haiku judge — fast/cheap, routine phases.
    Routine,
    /// Single Kimi K2-Thinking judge — the default review tier (Stage A
    /// behavior). OSS frontier, far cheaper than closed frontier for
    /// comparable adversarial-judge quality; callable on the operator's key.
    Review,
    /// Kimi + Sonnet in parallel. Cross-vendor pair: Eastern OSS +
    /// Western closed = different blind spots, real disagreement
    /// signal.
    Critical,
    /// Kimi + Sonnet + Opus trio. Highest-stakes work; majority
    /// vote when judges disagree.
    CriticalStrict,
    /// Single Kimi judge BUT records OpenRouter's resolved
    /// `model_id` + `provider` in the StepProof for audit-replay.
    /// EU AI Act Article 12 compliance: "prove which model judged
    /// this" over a 10-year retention window.
    AuditGrade,
}

impl Default for JudgeTrustTier {
    fn default() -> Self {
        Self::Review
    }
}

impl JudgeTrustTier {
    /// The list of judges to run for this tier. Order is stable —
    /// callers iterate in this order and disagreement messages
    /// reference the same indices.
    #[must_use]
    pub fn judge_models(self) -> Vec<JudgeModel> {
        match self {
            Self::Routine => vec![JudgeModel::Haiku],
            Self::Review | Self::AuditGrade => vec![JudgeModel::Kimi],
            Self::Critical => vec![JudgeModel::Kimi, JudgeModel::Sonnet],
            Self::CriticalStrict => {
                vec![JudgeModel::Kimi, JudgeModel::Sonnet, JudgeModel::Opus]
            }
        }
    }

    /// Whether this tier records the resolved provider in StepProof
    /// for audit-grade replay. Only `AuditGrade` does today.
    #[must_use]
    pub const fn records_provider(self) -> bool {
        matches!(self, Self::AuditGrade)
    }

    /// Whether this tier should produce a `Disagreement` chain entry
    /// when judges disagree. Single-judge tiers can't disagree, so
    /// returns `false`. Multi-judge tiers always log disagreements.
    #[must_use]
    pub fn logs_disagreement(self) -> bool {
        self.judge_models().len() > 1
    }
}

/// One judge's verdict, paired with the model that produced it.
/// Stored in `MultiJudgeVerdict.individuals` so a verifier (or the
/// dashboard) can see exactly which model said what.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeRun {
    pub model: JudgeModel,
    pub verdict: JudgeVerdict,
    /// Cost in USD for this single judge call. `None` when pricing
    /// accounting is disabled or the OpenRouter response didn't
    /// include cost headers. Sums across the parallel set in
    /// [`MultiJudgeVerdict::total_cost_usd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Resolved provider (e.g. `parasail-fp8`, `deepinfra/fp16`)
    /// from OpenRouter's response. `None` when not audit-grade.
    /// Audit-grade replay needs this to identify the exact
    /// inference path that produced the verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Synthesized verdict across N judge runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiJudgeVerdict {
    /// Worst-case across all individuals: `false` if ANY judge said
    /// `false`. See module docs for rationale.
    pub sufficient: bool,
    /// Floor (min) confidence across all individuals. The
    /// worst-case judge's number, not the average — averaging
    /// would mask uncertainty.
    pub confidence: f64,
    /// `true` iff at least one pair of judges disagrees on
    /// `sufficient`. Drives `ProofEntry::Disagreement` (follow-up
    /// commit) when the tier's `logs_disagreement()` is true.
    pub disagreement: bool,
    /// Tier this verdict was synthesised under. Recorded so a
    /// verifier can reconstruct the expected judge-model list.
    pub tier: JudgeTrustTier,
    /// Per-judge runs, in `JudgeTrustTier::judge_models()` order.
    pub individuals: Vec<JudgeRun>,
    /// Sum of per-judge `cost_usd` values. `None` when no individual
    /// run reported a cost; `Some(0.0)` when reported but zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
}

impl MultiJudgeVerdict {
    /// Synthesize from a list of `JudgeRun`s for a given tier. The
    /// caller is responsible for ensuring `runs.len()` matches
    /// `tier.judge_models().len()` — passing fewer runs than the
    /// tier expects is treated as data; the synthesis still runs but
    /// the resulting verdict carries whatever runs were provided.
    /// (Validation belongs at the producer layer, not here.)
    #[must_use]
    pub fn synthesize(tier: JudgeTrustTier, runs: Vec<JudgeRun>) -> Self {
        let sufficient = runs.iter().all(|r| r.verdict.sufficient);
        let disagreement =
            !runs.is_empty() && !runs.iter().all(|r| r.verdict.sufficient == sufficient);
        let confidence = runs
            .iter()
            .map(|r| r.verdict.confidence)
            .fold(f64::INFINITY, f64::min);
        let confidence = if confidence.is_infinite() {
            // Empty runs: degenerate case. Treat as 0.0 to be
            // conservative — "I have no judges, I can't claim any
            // confidence." Synthesizing a verdict from zero judges
            // is a producer bug; this is the safe answer.
            0.0
        } else {
            confidence
        };
        let total_cost_usd = if runs.iter().all(|r| r.cost_usd.is_none()) {
            None
        } else {
            Some(runs.iter().filter_map(|r| r.cost_usd).sum())
        };
        Self {
            sufficient,
            confidence,
            disagreement,
            tier,
            individuals: runs,
            total_cost_usd,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(model: JudgeModel, sufficient: bool, confidence: f64) -> JudgeRun {
        JudgeRun {
            model,
            verdict: if sufficient {
                JudgeVerdict::pass(confidence, "ok")
            } else {
                JudgeVerdict::fail(confidence, "no", vec![])
            },
            cost_usd: None,
            provider: None,
        }
    }

    fn run_with_cost(model: JudgeModel, sufficient: bool, confidence: f64, cost: f64) -> JudgeRun {
        let mut r = run(model, sufficient, confidence);
        r.cost_usd = Some(cost);
        r
    }

    #[test]
    fn tier_default_is_review() {
        assert_eq!(JudgeTrustTier::default(), JudgeTrustTier::Review);
    }

    #[test]
    fn judge_models_per_tier() {
        assert_eq!(
            JudgeTrustTier::Routine.judge_models(),
            vec![JudgeModel::Haiku]
        );
        assert_eq!(
            JudgeTrustTier::Review.judge_models(),
            vec![JudgeModel::Kimi]
        );
        assert_eq!(
            JudgeTrustTier::Critical.judge_models(),
            vec![JudgeModel::Kimi, JudgeModel::Sonnet]
        );
        assert_eq!(
            JudgeTrustTier::CriticalStrict.judge_models(),
            vec![JudgeModel::Kimi, JudgeModel::Sonnet, JudgeModel::Opus]
        );
        assert_eq!(
            JudgeTrustTier::AuditGrade.judge_models(),
            vec![JudgeModel::Kimi]
        );
    }

    #[test]
    fn audit_grade_records_provider_others_dont() {
        assert!(JudgeTrustTier::AuditGrade.records_provider());
        for t in [
            JudgeTrustTier::Routine,
            JudgeTrustTier::Review,
            JudgeTrustTier::Critical,
            JudgeTrustTier::CriticalStrict,
        ] {
            assert!(!t.records_provider(), "{t:?} should NOT record provider");
        }
    }

    #[test]
    fn logs_disagreement_only_for_multi_judge_tiers() {
        // Single-judge tiers can't disagree.
        assert!(!JudgeTrustTier::Routine.logs_disagreement());
        assert!(!JudgeTrustTier::Review.logs_disagreement());
        assert!(!JudgeTrustTier::AuditGrade.logs_disagreement());
        // Multi-judge tiers always log.
        assert!(JudgeTrustTier::Critical.logs_disagreement());
        assert!(JudgeTrustTier::CriticalStrict.logs_disagreement());
    }

    #[test]
    fn synthesize_all_pass_is_sufficient_no_disagreement() {
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                run(JudgeModel::Kimi, true, 0.95),
                run(JudgeModel::Sonnet, true, 0.90),
            ],
        );
        assert!(v.sufficient);
        assert!(!v.disagreement);
        // Floor (min), not average.
        assert!((v.confidence - 0.90).abs() < 1e-9);
    }

    #[test]
    fn synthesize_one_fail_flips_overall_to_fail() {
        // Worst-case-wins: one Fail among 2 Passes = overall Fail.
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::CriticalStrict,
            vec![
                run(JudgeModel::Kimi, false, 0.30),
                run(JudgeModel::Sonnet, true, 0.95),
                run(JudgeModel::Opus, true, 0.92),
            ],
        );
        assert!(!v.sufficient, "any single Fail must flip overall");
        assert!(v.disagreement, "Kimi disagrees with Sonnet+Opus");
        // Confidence is the worst-case judge's, not the failing
        // judge's — they happen to coincide here but the rule is
        // "min across all", not "the failing judge's value".
        assert!((v.confidence - 0.30).abs() < 1e-9);
    }

    #[test]
    fn synthesize_unanimous_fail_is_no_disagreement() {
        // All judges said `false` — the verdict is sufficient: false,
        // disagreement: false (everyone agrees on the failure).
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                run(JudgeModel::Kimi, false, 0.20),
                run(JudgeModel::Sonnet, false, 0.40),
            ],
        );
        assert!(!v.sufficient);
        assert!(!v.disagreement);
    }

    #[test]
    fn synthesize_empty_runs_is_safe_default() {
        // Producer bug: 0 runs for a Critical tier. Synthesis still
        // runs but conservatively: sufficient = vacuous-true (all of
        // an empty set), disagreement = false (no pairs), confidence
        // = 0.0 (we have no judges, we can't claim any).
        let v = MultiJudgeVerdict::synthesize(JudgeTrustTier::Critical, vec![]);
        assert!(v.sufficient); // Vacuous all() on empty iterator.
        assert!(!v.disagreement);
        assert_eq!(v.confidence, 0.0);
        assert_eq!(v.total_cost_usd, None);
    }

    #[test]
    fn synthesize_records_tier_for_replay() {
        // The verifier reconstructs expected judge models from
        // `tier`. If we lose this in synthesis, replay can't tell
        // whether 2 judges meant Critical or a half-failed
        // CriticalStrict.
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::CriticalStrict,
            vec![
                run(JudgeModel::Kimi, true, 0.9),
                run(JudgeModel::Sonnet, true, 0.9),
                run(JudgeModel::Opus, true, 0.9),
            ],
        );
        assert_eq!(v.tier, JudgeTrustTier::CriticalStrict);
        assert_eq!(v.individuals.len(), 3);
    }

    #[test]
    fn cost_aggregation_sums_when_any_individual_reports() {
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                run_with_cost(JudgeModel::Kimi, true, 0.9, 0.0023),
                run_with_cost(JudgeModel::Sonnet, true, 0.9, 0.0150),
            ],
        );
        let cost = v.total_cost_usd.unwrap();
        assert!((cost - 0.0173).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn cost_aggregation_none_when_no_individual_reports() {
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                run(JudgeModel::Kimi, true, 0.9),
                run(JudgeModel::Sonnet, true, 0.9),
            ],
        );
        assert_eq!(v.total_cost_usd, None);
    }

    #[test]
    fn cost_aggregation_partial_reports_sum_what_exists() {
        // Some judges report cost, others don't. Sum what we have;
        // missing reports treated as 0 (not None) so a single
        // priced judge still produces a meaningful aggregate.
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                run_with_cost(JudgeModel::Kimi, true, 0.9, 0.0023),
                run(JudgeModel::Sonnet, true, 0.9), // no cost
            ],
        );
        let cost = v.total_cost_usd.unwrap();
        assert!((cost - 0.0023).abs() < 1e-9);
    }

    #[test]
    fn tier_serde_kebab_case() {
        // Pin the wire format. Step config TOML carries
        // `trust_tier = "critical-strict"`; a future serde rename
        // would silently demote configs to the default tier.
        assert_eq!(
            serde_json::to_string(&JudgeTrustTier::Routine).unwrap(),
            "\"routine\""
        );
        assert_eq!(
            serde_json::to_string(&JudgeTrustTier::CriticalStrict).unwrap(),
            "\"critical-strict\""
        );
        assert_eq!(
            serde_json::to_string(&JudgeTrustTier::AuditGrade).unwrap(),
            "\"audit-grade\""
        );

        let parsed: JudgeTrustTier = serde_json::from_str("\"review\"").unwrap();
        assert_eq!(parsed, JudgeTrustTier::Review);
    }

    #[test]
    fn full_multi_judge_verdict_json_round_trip() {
        let v = MultiJudgeVerdict::synthesize(
            JudgeTrustTier::Critical,
            vec![
                run_with_cost(JudgeModel::Kimi, true, 0.95, 0.0023),
                run_with_cost(JudgeModel::Sonnet, false, 0.50, 0.0150),
            ],
        );
        let json = serde_json::to_string(&v).unwrap();
        let back: MultiJudgeVerdict = serde_json::from_str(&json).unwrap();
        assert!(!back.sufficient);
        assert!(back.disagreement);
        assert_eq!(back.individuals.len(), 2);
        assert_eq!(back.tier, JudgeTrustTier::Critical);
        let total = back.total_cost_usd.unwrap();
        assert!((total - 0.0173).abs() < 1e-9);
    }
}
