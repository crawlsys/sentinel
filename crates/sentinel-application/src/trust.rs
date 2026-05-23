//! Trust scoring from proof corpus history (M6.3 — Microsoft AGT pattern).
//!
//! Aggregates `ArchivedChainSummary` entries (from
//! [`crate::proof_archive::read_index`]) into a 0–1000 trust score with
//! five behavioral tiers, scoped per-skill and per-session (the
//! session_id is the agent identifier today). The output is the
//! cross-session corpus equivalent of "is this skill / agent
//! reliable enough to auto-approve at the `critical` trust tier?"
//! and feeds:
//!
//! - The Microsoft AGT borrowed pattern referenced in task #27.
//! - Pack contracts (#12 / orig M7.8): "auto-merge agents need
//!   Trusted+ tier" type policies. The contract layer reads tier; this
//!   module produces it.
//! - Dashboard #71/#72 surface: per-skill / per-agent score over time.
//!
//! # Why pure-application, not pure-domain
//!
//! [`crate::proof_archive::ArchivedChainSummary`] lives in this crate
//! because it carries timestamps and relates to FS-bound storage shape.
//! The scoring math itself is pure (no I/O, no async), so the module
//! sits in `sentinel-application` next to the data it operates on
//! rather than forcing a domain ↔ application data hop.
//!
//! # Inputs available *today*
//!
//! `ArchivedChainSummary` provides: skill, session_id, step_count,
//! phase_count, all_sufficient, head_hash, step_sequence, archived_at.
//! NOT yet on the summary: anomaly count (task #17 ships the hook,
//! the corpus rollup is follow-on), per-step cost (task #82 Stage B).
//! Once those land, fold them into [`score_for_skill`] /
//! [`score_for_session`] without changing the public API shape.

use crate::proof_archive::ArchivedChainSummary;
use serde::{Deserialize, Serialize};

/// Five-tier trust banding (Microsoft AGT borrowed shape).
///
/// Tier boundaries are intentionally conservative — a 100% pass rate on
/// 1 chain does NOT clear Probationary, because sample size is part of
/// the trust contract, not just success rate. See [`score_for_skill`]
/// for the full ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustTier {
    /// 0–200. New skill / new agent / failing skill. No auto-approval.
    Probationary,
    /// 201–500. Building track record, mixed signal. Manual review.
    Developing,
    /// 501–800. Reliable on routine work. Auto-approve at routine tier
    /// only.
    Established,
    /// 801–950. Strong cross-session track record. Auto-approve at
    /// review tier; critical tier still requires human gate.
    Trusted,
    /// 951–1000. Sustained near-perfect performance over hundreds of
    /// chains. Auto-approve at critical tier (unless step config sets
    /// `hardware_signing: required`).
    Verified,
}

impl TrustTier {
    /// Map a numeric score to its tier. Saturates at the band edges.
    #[must_use]
    pub const fn from_score(score: u16) -> Self {
        match score {
            0..=200 => Self::Probationary,
            201..=500 => Self::Developing,
            501..=800 => Self::Established,
            801..=950 => Self::Trusted,
            _ => Self::Verified,
        }
    }
}

/// One trust-score result for a (skill | session_id) bucket.
///
/// Carries the score + tier plus the inputs that produced it so the
/// dashboard / MCP responses can show the work, not just the verdict.
/// Fields are public for read-side consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustScore {
    /// 0–1000. See module-level docs for the formula.
    pub score: u16,
    /// Tier the score lands in. Always `TrustTier::from_score(score)`.
    pub tier: TrustTier,
    /// How many chains contributed to this score. Sample size is the
    /// honesty-check on `pass_rate` — a 100% pass on 1 chain is not the
    /// same as 100% on 200 chains.
    pub sample_size: usize,
    /// Fraction of contributing chains where every judge verdict was
    /// `sufficient`. Range [0.0, 1.0]. Cleanly 0.0 when sample_size = 0.
    pub pass_rate: f64,
    /// Average step count across contributing chains. Longer chains are
    /// not weighted higher in the *score* today, but the field is
    /// surfaced so callers can see whether this is a heavy-lift skill
    /// (deploy.ship-it: 30 steps) or routine (linear.claim: 3 steps).
    pub avg_step_count: f64,
}

/// Score every chain that matches `skill` from a corpus snapshot.
///
/// # Formula
///
/// 1. `pass_rate = sufficient_count / sample_size` (0.0 when empty).
/// 2. `base = (pass_rate * 1000.0).round() as u16`, clamped to `[0,
///    1000]`.
/// 3. **Sample-size cap**: a tiny corpus cannot clear higher tiers
///    regardless of pass rate — this is the load-bearing
///    "no-confidence-on-tiny-sample" invariant.
///    - sample_size == 0 → score 0 (Probationary).
///    - 1..=4 → cap at 200 (Probationary).
///    - 5..=20 → cap at 500 (Developing).
///    - 21..=100 → cap at 800 (Established).
///    - 101..=199 → cap at 950 (Trusted).
///    - 200+ → no cap (Verified is achievable).
///
/// The cap is intentionally aggressive at the low end — the whole point
/// of the trust score is to gate auto-approval, and auto-approving a
/// destructive step on the basis of "1 successful chain" is exactly the
/// failure mode this is meant to prevent.
#[must_use]
pub fn score_for_skill(skill: &str, summaries: &[ArchivedChainSummary]) -> TrustScore {
    let matching: Vec<&ArchivedChainSummary> =
        summaries.iter().filter(|s| s.skill == skill).collect();
    score_from_matches(&matching)
}

/// Same as [`score_for_skill`] but keys on `session_id` (the agent
/// identifier proxy until a richer agent abstraction exists).
#[must_use]
pub fn score_for_session(session_id: &str, summaries: &[ArchivedChainSummary]) -> TrustScore {
    let matching: Vec<&ArchivedChainSummary> = summaries
        .iter()
        .filter(|s| s.session_id == session_id)
        .collect();
    score_from_matches(&matching)
}

fn score_from_matches(matching: &[&ArchivedChainSummary]) -> TrustScore {
    let sample_size = matching.len();
    if sample_size == 0 {
        return TrustScore {
            score: 0,
            tier: TrustTier::Probationary,
            sample_size: 0,
            pass_rate: 0.0,
            avg_step_count: 0.0,
        };
    }

    let sufficient = matching.iter().filter(|s| s.all_sufficient).count();
    #[allow(clippy::cast_precision_loss)]
    let pass_rate = (sufficient as f64) / (sample_size as f64);
    #[allow(clippy::cast_precision_loss)]
    let avg_step_count =
        matching.iter().map(|s| s.step_count as f64).sum::<f64>() / (sample_size as f64);

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let base = ((pass_rate * 1000.0).round() as u16).min(1000);

    // Sample-size ceiling. The "no-confidence-on-tiny-sample" gate.
    let cap: u16 = match sample_size {
        0 => 0,
        1..=4 => 200,
        5..=20 => 500,
        21..=100 => 800,
        101..=199 => 950,
        _ => 1000,
    };
    let score = base.min(cap);

    TrustScore {
        score,
        tier: TrustTier::from_score(score),
        sample_size,
        pass_rate,
        avg_step_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn fake_summary(
        skill: &str,
        session_id: &str,
        sufficient: bool,
        steps: usize,
    ) -> ArchivedChainSummary {
        ArchivedChainSummary {
            schema_version: 1,
            session_id: session_id.to_string(),
            skill: skill.to_string(),
            step_count: steps,
            phase_count: 1,
            head_hash: "deadbeef".to_string(),
            all_sufficient: sufficient,
            step_sequence: vec![],
            archived_at: Utc.with_ymd_and_hms(2026, 5, 6, 0, 0, 0).unwrap(),
        }
    }

    #[test]
    fn empty_corpus_yields_probationary_zero() {
        let summaries: Vec<ArchivedChainSummary> = vec![];
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.score, 0);
        assert_eq!(s.tier, TrustTier::Probationary);
        assert_eq!(s.sample_size, 0);
        assert_eq!(s.pass_rate, 0.0);
    }

    #[test]
    fn one_perfect_chain_caps_at_probationary() {
        // The load-bearing invariant: 100% on n=1 is NOT verified.
        // Tiny samples cap at 200 even with a perfect record.
        let summaries = vec![fake_summary("linear", "sess-a", true, 3)];
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 1);
        assert_eq!(s.pass_rate, 1.0);
        assert_eq!(
            s.score, 200,
            "tiny-sample cap must hold base=1000 down to 200"
        );
        assert_eq!(s.tier, TrustTier::Probationary);
    }

    #[test]
    fn five_perfect_chains_unlocks_developing() {
        let summaries: Vec<ArchivedChainSummary> = (0..5)
            .map(|i| fake_summary("linear", &format!("sess-{i}"), true, 3))
            .collect();
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 5);
        assert_eq!(s.score, 500); // pass_rate=1.0 base 1000, capped to 500
        assert_eq!(s.tier, TrustTier::Developing);
    }

    #[test]
    fn twenty_one_perfect_chains_unlocks_established() {
        let summaries: Vec<ArchivedChainSummary> = (0..21)
            .map(|i| fake_summary("linear", &format!("sess-{i}"), true, 4))
            .collect();
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 21);
        assert_eq!(s.score, 800);
        assert_eq!(s.tier, TrustTier::Established);
        assert!((s.avg_step_count - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn one_hundred_one_perfect_chains_unlocks_trusted() {
        let summaries: Vec<ArchivedChainSummary> = (0..101)
            .map(|i| fake_summary("linear", &format!("s-{i}"), true, 5))
            .collect();
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 101);
        assert_eq!(s.score, 950);
        assert_eq!(s.tier, TrustTier::Trusted);
    }

    #[test]
    fn two_hundred_perfect_chains_unlocks_verified() {
        let summaries: Vec<ArchivedChainSummary> = (0..200)
            .map(|i| fake_summary("linear", &format!("s-{i}"), true, 5))
            .collect();
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 200);
        assert_eq!(s.score, 1000);
        assert_eq!(s.tier, TrustTier::Verified);
    }

    #[test]
    fn mixed_pass_fail_lowers_score() {
        // 21 chains, 18 pass / 3 fail = 85.7% pass rate.
        let mut summaries: Vec<ArchivedChainSummary> = (0..18)
            .map(|i| fake_summary("linear", &format!("p-{i}"), true, 3))
            .collect();
        summaries.extend((0..3).map(|i| fake_summary("linear", &format!("f-{i}"), false, 3)));
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 21);
        // base = round(0.857 * 1000) = 857; cap at this band = 800.
        assert_eq!(s.score, 800);
        assert_eq!(s.tier, TrustTier::Established);
        assert!((s.pass_rate - (18.0 / 21.0)).abs() < 1e-9);
    }

    #[test]
    fn all_failures_yields_probationary_zero_score() {
        let summaries: Vec<ArchivedChainSummary> = (0..50)
            .map(|i| fake_summary("linear", &format!("f-{i}"), false, 3))
            .collect();
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 50);
        assert_eq!(s.pass_rate, 0.0);
        assert_eq!(s.score, 0);
        assert_eq!(s.tier, TrustTier::Probationary);
    }

    #[test]
    fn skill_filter_excludes_other_skills() {
        let summaries = vec![
            fake_summary("linear", "a", true, 3),
            fake_summary("git", "b", false, 3),
            fake_summary("linear", "c", true, 3),
        ];
        // Only the 2 linear chains count, not the failing git chain.
        let s = score_for_skill("linear", &summaries);
        assert_eq!(s.sample_size, 2);
        assert_eq!(s.pass_rate, 1.0);
    }

    #[test]
    fn session_filter_groups_by_agent() {
        let summaries = vec![
            fake_summary("linear", "agent-a", true, 3),
            fake_summary("git", "agent-a", true, 3),
            fake_summary("linear", "agent-b", false, 3),
        ];
        let s = score_for_session("agent-a", &summaries);
        assert_eq!(s.sample_size, 2);
        assert_eq!(s.pass_rate, 1.0);
        let s_b = score_for_session("agent-b", &summaries);
        assert_eq!(s_b.sample_size, 1);
        assert_eq!(s_b.pass_rate, 0.0);
        assert_eq!(s_b.score, 0);
    }

    #[test]
    fn tier_from_score_band_edges() {
        assert_eq!(TrustTier::from_score(0), TrustTier::Probationary);
        assert_eq!(TrustTier::from_score(200), TrustTier::Probationary);
        assert_eq!(TrustTier::from_score(201), TrustTier::Developing);
        assert_eq!(TrustTier::from_score(500), TrustTier::Developing);
        assert_eq!(TrustTier::from_score(501), TrustTier::Established);
        assert_eq!(TrustTier::from_score(800), TrustTier::Established);
        assert_eq!(TrustTier::from_score(801), TrustTier::Trusted);
        assert_eq!(TrustTier::from_score(950), TrustTier::Trusted);
        assert_eq!(TrustTier::from_score(951), TrustTier::Verified);
        assert_eq!(TrustTier::from_score(1000), TrustTier::Verified);
    }

    #[test]
    fn tier_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&TrustTier::Probationary).unwrap(),
            "\"probationary\""
        );
        assert_eq!(
            serde_json::to_string(&TrustTier::Verified).unwrap(),
            "\"verified\""
        );
    }
}
