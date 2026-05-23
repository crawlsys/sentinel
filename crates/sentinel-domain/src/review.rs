//! Multi-lens code review verdict types (M3.3 / task #64).
//!
//! Storybloq-inspired pattern. Instead of one monolithic
//! "code-reviewer agent passed / failed" answer, the review fans
//! out across N concurrent lenses — security, performance, style,
//! tests, correctness — each returning its own structured
//! [`LensVerdict`]. A synthesis step combines them into a
//! [`MultiLensReview`] with an overall outcome.
//!
//! These types live in `sentinel-domain` so the M7 router-as-planner
//! can deserialize review output mechanically — without parsing the
//! agent's free-text response — and decide what to do next based on
//! per-lens severities. The agents-mcp-rust crate consumes them on
//! the producer side; the sentinel router consumes them on the
//! reader side.
//!
//! # Design principles
//!
//! 1. **Worst-case overall**. The orchestrator should never miss a
//!    `Fail` because some other lens passed. [`MultiLensReview::synthesize`]
//!    picks the worst per-lens outcome and the highest severity.
//! 2. **Empty review is a Pass**. A `MultiLensReview` with zero
//!    lenses synthesises to `Pass / Info / "no lenses run"`. This is
//!    counterintuitive but correct: refusing to synthesize empty
//!    input would conflate "no review yet" with "no findings."
//!    Callers that care about coverage check `lenses.is_empty()`
//!    before treating the verdict as authoritative.
//! 3. **Severity is independent of outcome**. A `Pass` with `Critical`
//!    severity is meaningful — "I found a critical issue and someone
//!    already fixed it; here's the evidence." A `Fail` with `Info`
//!    severity is also meaningful — "this isn't a release blocker
//!    but it's wrong." The two axes don't constrain each other.

use serde::{Deserialize, Serialize};

/// Outcome of a single lens, three-valued so "concern" can flag
/// follow-up work without blocking the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LensOutcome {
    /// Lens found no actionable issues.
    Pass,
    /// Lens found something worth attention but not a blocker.
    Concern,
    /// Lens found a blocker — chain should not advance.
    Fail,
}

impl LensOutcome {
    /// Rank for worst-of comparisons. Larger = worse.
    const fn rank(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Concern => 1,
            Self::Fail => 2,
        }
    }
}

/// Severity scale for findings. Independent of [`LensOutcome`] —
/// see module-level docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    /// Rank for worst-of comparisons. Larger = worse.
    const fn rank(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Critical => 4,
        }
    }
}

/// One lens's review of the change. Carried in
/// [`MultiLensReview::lenses`] so the orchestrator can show a
/// per-lens table without re-parsing the agent's text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LensVerdict {
    /// Lens identifier — one of "security", "performance", "style",
    /// "tests", "correctness", or any other registered lens.
    pub lens: String,
    pub outcome: LensOutcome,
    pub severity: Severity,
    /// Evidence the lens collected — file paths + line numbers,
    /// snippets, test output, etc. Free-form strings.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Plain-text reasoning the agent emitted for the chain log.
    pub reasoning: String,
}

/// Synthesis of N [`LensVerdict`]s into one verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiLensReview {
    /// Worst-case across all lenses. See [`MultiLensReview::synthesize`].
    pub overall: LensOutcome,
    /// Worst severity across all lenses, again worst-case.
    pub overall_severity: Severity,
    /// Every lens's verdict, in registration order.
    pub lenses: Vec<LensVerdict>,
    /// Human-readable summary line. The orchestrator usually
    /// generates this; M3.3's `synthesize()` produces a default.
    pub summary: String,
}

impl MultiLensReview {
    /// Build a `MultiLensReview` from a list of lens verdicts using
    /// worst-case-wins synthesis.
    ///
    /// - **overall**: highest-rank outcome across all lenses
    ///   (`Fail` > `Concern` > `Pass`). Empty input → `Pass`.
    /// - **overall_severity**: highest-rank severity across all
    ///   lenses (`Critical` > `High` > `Medium` > `Low` > `Info`).
    ///   Empty input → `Info`.
    /// - **summary**: counts of each outcome ("3 lenses: 1 fail, 2
    ///   pass") for at-a-glance read; M3.3 leaves this minimal so
    ///   richer prose synthesis can land later without reshaping
    ///   the API.
    #[must_use]
    pub fn synthesize(lenses: Vec<LensVerdict>) -> Self {
        let overall = lenses
            .iter()
            .map(|v| v.outcome)
            .max_by_key(|o| o.rank())
            .unwrap_or(LensOutcome::Pass);
        let overall_severity = lenses
            .iter()
            .map(|v| v.severity)
            .max_by_key(|s| s.rank())
            .unwrap_or(Severity::Info);
        let summary = build_summary(&lenses);
        Self {
            overall,
            overall_severity,
            lenses,
            summary,
        }
    }
}

fn build_summary(lenses: &[LensVerdict]) -> String {
    if lenses.is_empty() {
        return "no lenses run".to_string();
    }
    let mut pass = 0usize;
    let mut concern = 0usize;
    let mut fail = 0usize;
    for v in lenses {
        match v.outcome {
            LensOutcome::Pass => pass += 1,
            LensOutcome::Concern => concern += 1,
            LensOutcome::Fail => fail += 1,
        }
    }
    let total = lenses.len();
    let mut parts = Vec::new();
    if fail > 0 {
        parts.push(format!("{fail} fail"));
    }
    if concern > 0 {
        parts.push(format!("{concern} concern"));
    }
    if pass > 0 {
        parts.push(format!("{pass} pass"));
    }
    format!(
        "{total} lens{}: {}",
        if total == 1 { "" } else { "es" },
        parts.join(", ")
    )
}

/// The canonical lens set. Agents-mcp-rust uses this when fanning
/// out a multi-lens review request; the M7 router uses it to
/// validate that all expected lenses ran.
pub const DEFAULT_LENSES: &[&str] = &["security", "performance", "style", "tests", "correctness"];

#[cfg(test)]
mod tests {
    use super::*;

    fn v(lens: &str, outcome: LensOutcome, severity: Severity) -> LensVerdict {
        LensVerdict {
            lens: lens.to_string(),
            outcome,
            severity,
            evidence: Vec::new(),
            reasoning: "x".to_string(),
        }
    }

    #[test]
    fn synthesize_empty_review_is_pass_info() {
        let r = MultiLensReview::synthesize(Vec::new());
        assert_eq!(r.overall, LensOutcome::Pass);
        assert_eq!(r.overall_severity, Severity::Info);
        assert_eq!(r.summary, "no lenses run");
    }

    #[test]
    fn synthesize_all_pass_is_pass() {
        let r = MultiLensReview::synthesize(vec![
            v("security", LensOutcome::Pass, Severity::Info),
            v("style", LensOutcome::Pass, Severity::Low),
        ]);
        assert_eq!(r.overall, LensOutcome::Pass);
        assert_eq!(r.overall_severity, Severity::Low);
    }

    #[test]
    fn synthesize_one_fail_is_fail_overall() {
        // Worst-case wins. One Fail among many Passes → Fail.
        let r = MultiLensReview::synthesize(vec![
            v("security", LensOutcome::Fail, Severity::High),
            v("performance", LensOutcome::Pass, Severity::Info),
            v("style", LensOutcome::Pass, Severity::Low),
        ]);
        assert_eq!(r.overall, LensOutcome::Fail);
        assert_eq!(r.overall_severity, Severity::High);
    }

    #[test]
    fn synthesize_concern_only_is_concern() {
        let r = MultiLensReview::synthesize(vec![
            v("performance", LensOutcome::Concern, Severity::Medium),
            v("style", LensOutcome::Pass, Severity::Low),
        ]);
        assert_eq!(r.overall, LensOutcome::Concern);
        assert_eq!(r.overall_severity, Severity::Medium);
    }

    #[test]
    fn severity_independent_of_outcome() {
        // A Pass with Critical severity is valid: "I found something
        // critical and someone fixed it." Synthesis must preserve
        // both axes, not collapse Critical → Fail.
        let r =
            MultiLensReview::synthesize(vec![v("security", LensOutcome::Pass, Severity::Critical)]);
        assert_eq!(r.overall, LensOutcome::Pass);
        assert_eq!(r.overall_severity, Severity::Critical);
    }

    #[test]
    fn fail_takes_severity_from_worst_lens_not_failing_lens() {
        // The Fail lens has Low severity; another Pass lens has High
        // severity. Severity is across-lens worst-case, not the
        // failing lens's severity. This decouples the axes.
        let r = MultiLensReview::synthesize(vec![
            v("style", LensOutcome::Fail, Severity::Low),
            v("security", LensOutcome::Pass, Severity::High),
        ]);
        assert_eq!(r.overall, LensOutcome::Fail);
        assert_eq!(r.overall_severity, Severity::High);
    }

    #[test]
    fn summary_format_pluralises_correctly() {
        let one = MultiLensReview::synthesize(vec![v("style", LensOutcome::Pass, Severity::Info)]);
        assert_eq!(one.summary, "1 lens: 1 pass");

        let many = MultiLensReview::synthesize(vec![
            v("a", LensOutcome::Fail, Severity::High),
            v("b", LensOutcome::Concern, Severity::Medium),
            v("c", LensOutcome::Pass, Severity::Low),
        ]);
        assert_eq!(many.summary, "3 lenses: 1 fail, 1 concern, 1 pass");
    }

    #[test]
    fn lens_outcome_serde_round_trip() {
        for o in [LensOutcome::Pass, LensOutcome::Concern, LensOutcome::Fail] {
            let s = serde_json::to_string(&o).unwrap();
            let back: LensOutcome = serde_json::from_str(&s).unwrap();
            assert_eq!(o, back);
        }
    }

    #[test]
    fn severity_serde_round_trip() {
        for sev in [
            Severity::Info,
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            let s = serde_json::to_string(&sev).unwrap();
            let back: Severity = serde_json::from_str(&s).unwrap();
            assert_eq!(sev, back);
        }
    }

    #[test]
    fn kebab_case_wire_format_is_pinned() {
        // M7 router-as-planner deserializes these JSON shapes —
        // pinning the kebab-case wire format prevents a future serde
        // rename from silently breaking downstream consumers.
        assert_eq!(
            serde_json::to_string(&LensOutcome::Pass).unwrap(),
            "\"pass\""
        );
        assert_eq!(
            serde_json::to_string(&LensOutcome::Concern).unwrap(),
            "\"concern\""
        );
        assert_eq!(
            serde_json::to_string(&LensOutcome::Fail).unwrap(),
            "\"fail\""
        );
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            "\"critical\""
        );
    }

    #[test]
    fn full_review_json_round_trip() {
        let r = MultiLensReview::synthesize(vec![v(
            "security",
            LensOutcome::Concern,
            Severity::Medium,
        )]);
        let json = serde_json::to_string(&r).unwrap();
        let back: MultiLensReview = serde_json::from_str(&json).unwrap();
        assert_eq!(back.overall, LensOutcome::Concern);
        assert_eq!(back.overall_severity, Severity::Medium);
        assert_eq!(back.lenses.len(), 1);
        assert_eq!(back.lenses[0].lens, "security");
    }

    #[test]
    fn default_lenses_set_is_what_router_expects() {
        // Pin the canonical set — adding a lens here is a deliberate
        // design choice, not a drive-by edit. The M7 router-as-planner
        // checks `lenses.len() == DEFAULT_LENSES.len()` to validate
        // coverage, so silently adding a 6th lens would break every
        // existing pack until they opt in.
        assert_eq!(DEFAULT_LENSES.len(), 5);
        assert!(DEFAULT_LENSES.contains(&"security"));
        assert!(DEFAULT_LENSES.contains(&"performance"));
        assert!(DEFAULT_LENSES.contains(&"style"));
        assert!(DEFAULT_LENSES.contains(&"tests"));
        assert!(DEFAULT_LENSES.contains(&"correctness"));
    }
}
