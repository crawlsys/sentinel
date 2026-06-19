//! A13 Phase 3 — `spec_challenge_gate` `PreToolUse` hook.
//!
//! Reads the agent's [`SpecChallenge`] from
//! `input.extra.spec_challenge`, runs the deterministic
//! completeness check, optionally queries a semantic
//! [`SpecChallengeScorerPort`] for Catastrophic-class work, and
//! persists the challenge via [`SpecChallengeStorePort`] for
//! proof-chain re-verification.
//!
//! ## Trigger
//!
//! The hook fires for every `PreToolUse`, but its behavior is
//! reversibility-class-graded per spec §3:
//!
//! | Class | Behavior |
//! |---|---|
//! | `TriviallyReversible` | skip (allow); challenge cost not justified |
//! | `ReversibleWithEffort` | optional (allow whether challenge present or not) |
//! | `Irreversible` | require challenge present + completeness-clean |
//! | `Catastrophic` | require challenge + completeness + scorer all-axes-above-threshold |
//!
//! ## Modes
//!
//! Operators flip [`A13EnforcementMode`] to ratchet through rollout:
//!
//! - `ObserveOnly` — never blocks; findings logged for telemetry.
//! - `DefaultBlocking` — class-graded per the table above.
//! - `StrictBlocking` — `Irreversible` is also scored (treated as
//!   Catastrophic semantically). Used in high-stakes domains.
//!
//! The wiring layer (Phase 5) determines `class` via the existing
//! `ReversibilityClassifierPort`; this hook accepts the class as an
//! argument so it stays unit-testable in isolation.

use sentinel_domain::events::{HookInput, HookOutput};
use sentinel_domain::ports::{SpecChallengeScore, SpecChallengeScorerPort, SpecChallengeStorePort};
use sentinel_domain::reversibility::ReversibilityClass;
use sentinel_domain::spec_challenge::{ChallengeCategoryName, SpecChallenge};

/// Operator-facing enforcement strictness. Same shape as
/// `provenance_validate::ValidationMode` so the BA-vertical config
/// surface stays uniform across A13 + BA1 + BA3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A13EnforcementMode {
    /// Never blocks. Every check still runs; findings are logged
    /// at info level. Rollout posture.
    ObserveOnly,
    /// Class-graded per spec §3 — see the table at the module doc.
    DefaultBlocking,
    /// `Irreversible` work is also subject to the scorer + axis-
    /// threshold check (treated semantically as Catastrophic).
    StrictBlocking,
}

impl A13EnforcementMode {
    /// Returns `true` iff this mode can ever return a deny.
    #[must_use]
    pub const fn allows_blocking(self) -> bool {
        !matches!(self, Self::ObserveOnly)
    }
}

/// Default minimum axis score required for the scorer to pass a
/// Catastrophic-class challenge. Operator overrides per call via
/// [`process_with_threshold`].
pub const DEFAULT_CATASTROPHIC_AXIS_THRESHOLD: f32 = 0.7;

#[derive(Debug, Clone)]
pub struct SpecChallengeEvaluation {
    pub class: ReversibilityClass,
    pub mode: A13EnforcementMode,
    pub catastrophic_axis_threshold: f32,
    pub challenge: Option<SpecChallenge>,
    pub malformed_challenge: bool,
    pub missing_required_challenge: bool,
    pub completeness_finding_count: usize,
    pub store_error: Option<String>,
    pub scoring_required: bool,
    pub scorer_missing: bool,
    pub scorer_error: Option<String>,
    pub score: Option<SpecChallengeScore>,
    pub scorer_rejected: bool,
    pub would_block: bool,
    pub should_block: bool,
    pub block_reason: Option<String>,
}

#[must_use]
pub fn is_a13_signal(input: &HookInput, class: ReversibilityClass) -> bool {
    !matches!(class, ReversibilityClass::TriviallyReversible)
        && (input.extra.contains_key("spec_challenge")
            || matches!(
                class,
                ReversibilityClass::Irreversible | ReversibilityClass::Catastrophic
            ))
}

/// Process a `PreToolUse` event through the A13 spec-challenge
/// gate. Uses the [`DEFAULT_CATASTROPHIC_AXIS_THRESHOLD`].
#[must_use]
pub fn process(
    input: &HookInput,
    class: ReversibilityClass,
    store: Option<&dyn SpecChallengeStorePort>,
    scorer: Option<&dyn SpecChallengeScorerPort>,
    mode: A13EnforcementMode,
) -> HookOutput {
    process_with_threshold(
        input,
        class,
        store,
        scorer,
        mode,
        DEFAULT_CATASTROPHIC_AXIS_THRESHOLD,
    )
}

/// [`process`] with an explicit axis threshold for the scorer check.
#[must_use]
pub fn process_with_threshold(
    input: &HookInput,
    class: ReversibilityClass,
    store: Option<&dyn SpecChallengeStorePort>,
    scorer: Option<&dyn SpecChallengeScorerPort>,
    mode: A13EnforcementMode,
    catastrophic_axis_threshold: f32,
) -> HookOutput {
    let Some(evaluation) = evaluate_with_threshold(
        input,
        class,
        store,
        scorer,
        mode,
        catastrophic_axis_threshold,
    ) else {
        return HookOutput::allow();
    };
    output_from_evaluation(&evaluation)
}

#[must_use]
pub fn evaluate_with_threshold(
    input: &HookInput,
    class: ReversibilityClass,
    store: Option<&dyn SpecChallengeStorePort>,
    scorer: Option<&dyn SpecChallengeScorerPort>,
    mode: A13EnforcementMode,
    catastrophic_axis_threshold: f32,
) -> Option<SpecChallengeEvaluation> {
    // 1. TriviallyReversible: skip regardless of mode. Challenge
    // cost isn't justified for trivial actions.
    if matches!(class, ReversibilityClass::TriviallyReversible) {
        return None;
    }

    // 2. Parse the challenge artifact (if present). Missing-but-
    // expected and malformed get distinct error messages.
    let challenge_value = input.extra.get("spec_challenge");

    let challenge = match challenge_value {
        None => return evaluate_missing_challenge(class, mode, catastrophic_axis_threshold),
        Some(value) => match serde_json::from_value::<SpecChallenge>(value.clone()) {
            Ok(c) => c,
            Err(err) => {
                let reason = format!(
                    "spec_challenge field is malformed: {err}. \
                     The agent must emit a complete SpecChallenge JSON for {class:?} work."
                );
                tracing::warn!(
                    error = %err,
                    "spec_challenge_gate: malformed challenge artifact"
                );
                let would_block = true;
                return Some(SpecChallengeEvaluation {
                    class,
                    mode,
                    catastrophic_axis_threshold,
                    challenge: None,
                    malformed_challenge: true,
                    missing_required_challenge: false,
                    completeness_finding_count: 0,
                    store_error: None,
                    scoring_required: false,
                    scorer_missing: false,
                    scorer_error: None,
                    score: None,
                    scorer_rejected: false,
                    would_block,
                    should_block: mode.allows_blocking() && would_block,
                    block_reason: Some(reason),
                });
            }
        },
    };

    let mut evaluation = SpecChallengeEvaluation {
        class,
        mode,
        catastrophic_axis_threshold,
        challenge: Some(challenge.clone()),
        malformed_challenge: false,
        missing_required_challenge: false,
        completeness_finding_count: 0,
        store_error: None,
        scoring_required: false,
        scorer_missing: false,
        scorer_error: None,
        score: None,
        scorer_rejected: false,
        would_block: false,
        should_block: false,
        block_reason: None,
    };

    // 3. Completeness check (deterministic floor).
    let findings = challenge.completeness_findings();
    if !findings.is_empty() {
        let reason = format_completeness_failure(&challenge, &findings);
        evaluation.completeness_finding_count = findings.len();
        evaluation.would_block = true;
        evaluation.block_reason = Some(reason);
        if mode.allows_blocking() {
            evaluation.should_block = true;
            return Some(evaluation);
        }
        tracing::info!(
            count = findings.len(),
            "spec_challenge_gate: would block on incomplete challenge (ObserveOnly)"
        );
    }

    // 4. Persistence. For enforced A13 work, a failed challenge write is a
    // finding: the agent cannot prove what it challenged after acting.
    if let Some(store) = store {
        if let Err(err) = store.save(&challenge) {
            let reason = format!(
                "spec_challenge_gate: failed to persist SpecChallenge for work_id={} — {err}. \
                 Refusing to continue without durable A13 audit evidence.",
                challenge.work_id.as_str()
            );
            tracing::warn!(
                work_id = challenge.work_id.as_str(),
                error = %err,
                "spec_challenge_gate: store save failed"
            );
            evaluation.store_error = Some(err.to_string());
            evaluation.would_block = true;
            if evaluation.block_reason.is_none() {
                evaluation.block_reason = Some(reason);
            }
            if mode.allows_blocking() {
                evaluation.should_block = true;
                return Some(evaluation);
            }
        }
    }

    // 5. Scorer (only for Catastrophic, or Irreversible under
    // StrictBlocking).
    let needs_scoring = matches!(class, ReversibilityClass::Catastrophic)
        || (matches!(class, ReversibilityClass::Irreversible)
            && matches!(mode, A13EnforcementMode::StrictBlocking));
    evaluation.scoring_required = needs_scoring;

    if needs_scoring {
        if !mode.allows_blocking() {
            // ObserveOnly with Catastrophic class: log + allow.
            tracing::info!(
                work_id = challenge.work_id.as_str(),
                "spec_challenge_gate: would score Catastrophic challenge (ObserveOnly)"
            );
            return Some(evaluation);
        }

        let Some(scorer) = scorer else {
            evaluation.scorer_missing = true;
            evaluation.would_block = true;
            evaluation.block_reason = Some(format!(
                "spec_challenge_gate: {class:?} work requires semantic scoring but \
                 no scorer is configured. Wire a `SpecChallengeScorerPort` adapter \
                 or downgrade the upcoming work's reversibility class.",
            ));
            evaluation.should_block = mode.allows_blocking();
            return Some(evaluation);
        };

        let judged = match scorer.score(&challenge) {
            Ok(s) => s,
            Err(err) => {
                evaluation.scorer_error = Some(err.to_string());
                evaluation.would_block = true;
                evaluation.block_reason = Some(format!(
                    "spec_challenge_gate: scorer error for {class:?} work — {err}. \
                     Retry after the scorer recovers.",
                ));
                evaluation.should_block = mode.allows_blocking();
                return Some(evaluation);
            }
        };
        evaluation.score = Some(judged);

        if !judged.all_axes_above(catastrophic_axis_threshold) {
            evaluation.scorer_rejected = true;
            evaluation.would_block = true;
            evaluation.block_reason = Some(format!(
                "spec_challenge_gate: scorer rejected {class:?} challenge — \
                 weakest axis {weak:.2} below threshold {th:.2}. \
                 Per-axis scores: assumptions={a:.2}, gaps={g:.2}, ambiguities={am:.2}, \
                 alternatives_considered={alt:.2}, constraints_not_satisfied={c:.2}.",
                weak = judged.min_axis(),
                th = catastrophic_axis_threshold,
                a = judged.assumptions,
                g = judged.gaps,
                am = judged.ambiguities,
                alt = judged.alternatives_considered,
                c = judged.constraints_not_satisfied,
            ));
            evaluation.should_block = mode.allows_blocking();
            return Some(evaluation);
        }
    }

    evaluation.should_block = mode.allows_blocking() && evaluation.would_block;
    Some(evaluation)
}

#[must_use]
pub fn output_from_evaluation(evaluation: &SpecChallengeEvaluation) -> HookOutput {
    if evaluation.should_block {
        HookOutput::deny(
            evaluation
                .block_reason
                .clone()
                .unwrap_or_else(|| "spec_challenge_gate: blocked by A13 evaluation".to_string()),
        )
    } else {
        HookOutput::allow()
    }
}

fn evaluate_missing_challenge(
    class: ReversibilityClass,
    mode: A13EnforcementMode,
    catastrophic_axis_threshold: f32,
) -> Option<SpecChallengeEvaluation> {
    match class {
        ReversibilityClass::TriviallyReversible | ReversibilityClass::ReversibleWithEffort => None,
        ReversibilityClass::Irreversible | ReversibilityClass::Catastrophic => {
            let reason = format!(
                "spec_challenge_gate: {class:?} work requires a SpecChallenge artifact \
                 in `extra.spec_challenge`. The agent must articulate its assumptions, \
                 gaps, ambiguities, alternatives_considered, and constraints_not_satisfied \
                 before acting."
            );
            if !mode.allows_blocking() {
                tracing::info!(
                    ?class,
                    "spec_challenge_gate: would block missing challenge (ObserveOnly)"
                );
            }
            let would_block = true;
            Some(SpecChallengeEvaluation {
                class,
                mode,
                catastrophic_axis_threshold,
                challenge: None,
                malformed_challenge: false,
                missing_required_challenge: true,
                completeness_finding_count: 0,
                store_error: None,
                scoring_required: false,
                scorer_missing: false,
                scorer_error: None,
                score: None,
                scorer_rejected: false,
                would_block,
                should_block: mode.allows_blocking() && would_block,
                block_reason: Some(reason),
            })
        }
    }
}

fn format_completeness_failure(
    challenge: &SpecChallenge,
    findings: &[sentinel_domain::spec_challenge::CompletenessFinding],
) -> String {
    use sentinel_domain::spec_challenge::CompletenessFinding;
    use std::fmt::Write;
    let mut buf = format!(
        "spec_challenge_gate: challenge for work_id={} is incomplete ({} findings):\n",
        challenge.work_id.as_str(),
        findings.len(),
    );
    for finding in findings {
        match finding {
            CompletenessFinding::SilentEmpty { category } => {
                let _ = writeln!(
                    buf,
                    "  - silent-empty category `{}` (must have items OR an explicit \
                     `assertion_of_none` reason)",
                    category_key(*category),
                );
            }
            CompletenessFinding::InsufficientInterpretations {
                excerpt_preview,
                count,
            } => {
                let _ = writeln!(
                    buf,
                    "  - ambiguity with {count} interpretation(s) (need ≥ 2): \
                     {excerpt_preview:?}"
                );
            }
            CompletenessFinding::InferenceWithoutSource { topic } => {
                let _ = writeln!(
                    buf,
                    "  - gap {topic:?} resolved via `InferredFromContext` without an \
                     `inference_source`"
                );
            }
        }
    }
    buf
}

const fn category_key(c: ChallengeCategoryName) -> &'static str {
    c.key()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use sentinel_domain::ports::{
        SpecChallengeScore, SpecChallengeScorerError, SpecChallengeStoreError,
    };
    use sentinel_domain::reversibility::ReversibilityClass;
    use sentinel_domain::spec_challenge::{
        Alternative, Ambiguity, Assumption, AssumptionConfidence, ChallengeCategory, GapResolution,
        SpecChallenge, SpecGap, SpecReference, WorkId,
    };
    use std::sync::Mutex;

    fn well_formed_challenge() -> SpecChallenge {
        SpecChallenge {
            work_id: WorkId::new("w1").unwrap(),
            agent_id: "agent-x".to_string(),
            challenged_spec: SpecReference {
                hash: "abc".to_string(),
                source: "issue X".to_string(),
            },
            reversibility_class: ReversibilityClass::Irreversible,
            assumptions: ChallengeCategory::new(vec![Assumption {
                statement: "postgres".to_string(),
                confidence: AssumptionConfidence::High,
                blast_if_wrong: ReversibilityClass::Irreversible,
            }]),
            gaps: ChallengeCategory::new(vec![SpecGap {
                topic: "auth".to_string(),
                how_resolved: GapResolution::OperatorClarified,
                inference_source: None,
            }]),
            ambiguities: ChallengeCategory::new(vec![Ambiguity {
                spec_excerpt: "ship fast".to_string(),
                interpretations: vec!["p99".to_string(), "throughput".to_string()],
                chosen: "p99".to_string(),
                rationale: "user-visible".to_string(),
            }]),
            alternatives_considered: ChallengeCategory::new(vec![Alternative {
                description: "redis".to_string(),
                why_rejected: "durability".to_string(),
            }]),
            constraints_not_satisfied: ChallengeCategory::none("all met"),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        }
    }

    fn input_with_challenge(challenge: &SpecChallenge) -> HookInput {
        let mut input = HookInput::default();
        input.extra.insert(
            "spec_challenge".to_string(),
            serde_json::to_value(challenge).unwrap(),
        );
        input
    }

    fn input_without_challenge() -> HookInput {
        HookInput::default()
    }

    struct RecordingStore {
        saved: Mutex<Vec<SpecChallenge>>,
        fail: bool,
    }

    impl RecordingStore {
        fn new() -> Self {
            Self {
                saved: Mutex::new(Vec::new()),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                saved: Mutex::new(Vec::new()),
                fail: true,
            }
        }
    }

    impl SpecChallengeStorePort for RecordingStore {
        fn save(&self, challenge: &SpecChallenge) -> Result<(), SpecChallengeStoreError> {
            if self.fail {
                return Err(SpecChallengeStoreError::StoreUnavailable("disk".into()));
            }
            self.saved.lock().unwrap().push(challenge.clone());
            Ok(())
        }
        fn load(&self, _: &WorkId) -> Result<Option<SpecChallenge>, SpecChallengeStoreError> {
            Ok(self.saved.lock().unwrap().last().cloned())
        }
    }

    struct StubScorer {
        result: Result<SpecChallengeScore, SpecChallengeScorerError>,
    }

    impl SpecChallengeScorerPort for StubScorer {
        fn score(&self, _: &SpecChallenge) -> Result<SpecChallengeScore, SpecChallengeScorerError> {
            self.result.clone()
        }
    }

    // Helper for reading the deny message out of the output.
    fn deny_reason(output: &HookOutput) -> Option<&str> {
        output
            .hook_specific_output
            .as_ref()
            .and_then(|s| s.permission_decision_reason.as_deref())
            .or(output.reason.as_deref())
    }

    fn is_deny(output: &HookOutput) -> bool {
        use sentinel_domain::events::PermissionDecision;
        let hsp_deny = output
            .hook_specific_output
            .as_ref()
            .and_then(|s| s.permission_decision.as_ref())
            .is_some_and(|d| matches!(d, PermissionDecision::Deny));
        hsp_deny || output.blocked == Some(true)
    }

    #[test]
    fn trivially_reversible_always_allows() {
        let input = input_without_challenge();
        let out = process(
            &input,
            ReversibilityClass::TriviallyReversible,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(!is_deny(&out));
    }

    #[test]
    fn reversible_with_effort_allows_when_challenge_missing() {
        let input = input_without_challenge();
        let out = process(
            &input,
            ReversibilityClass::ReversibleWithEffort,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(!is_deny(&out));
    }

    #[test]
    fn irreversible_denies_when_challenge_missing() {
        let input = input_without_challenge();
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        let reason = deny_reason(&out).unwrap();
        assert!(reason.contains("requires a SpecChallenge"));
    }

    #[test]
    fn catastrophic_denies_when_challenge_missing() {
        let input = input_without_challenge();
        let out = process(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
    }

    #[test]
    fn observe_only_allows_missing_irreversible_challenge() {
        let input = input_without_challenge();
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            None,
            A13EnforcementMode::ObserveOnly,
        );
        assert!(!is_deny(&out));
    }

    #[test]
    fn malformed_challenge_denies_under_default_blocking() {
        let mut input = HookInput::default();
        input.extra.insert(
            "spec_challenge".to_string(),
            serde_json::json!("not an object"),
        );
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        let reason = deny_reason(&out).unwrap();
        assert!(reason.contains("malformed"));
    }

    #[test]
    fn complete_challenge_allows_irreversible_without_scorer() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(!is_deny(&out), "got reason: {:?}", deny_reason(&out));
    }

    #[test]
    fn incomplete_challenge_denies_naming_categories() {
        let mut challenge = well_formed_challenge();
        challenge.assumptions = ChallengeCategory::new(vec![]); // silent-empty
        let input = input_with_challenge(&challenge);
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        let reason = deny_reason(&out).unwrap();
        assert!(reason.contains("incomplete"), "got {reason}");
        assert!(
            reason.contains("assumptions"),
            "should name the failing category; got {reason}"
        );
    }

    #[test]
    fn store_persists_complete_challenge() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let store = RecordingStore::new();
        let _ = process(
            &input,
            ReversibilityClass::Irreversible,
            Some(&store),
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert_eq!(store.saved.lock().unwrap().len(), 1);
    }

    #[test]
    fn store_failure_blocks_enforced_allow_path() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let store = RecordingStore::failing();
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            Some(&store),
            None,
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        assert!(
            deny_reason(&out)
                .is_some_and(|reason| reason.contains("failed to persist SpecChallenge")),
            "store failure should surface durable-audit refusal: {:?}",
            deny_reason(&out)
        );
    }

    #[test]
    fn catastrophic_denies_when_no_scorer_configured() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let out = process(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            None, // no scorer
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        let reason = deny_reason(&out).unwrap();
        assert!(reason.contains("requires semantic scoring"));
    }

    #[test]
    fn catastrophic_allows_when_scorer_returns_high_scores() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let scorer = StubScorer {
            result: Ok(SpecChallengeScore::new(0.9, 0.9, 0.9, 0.9, 0.9)),
        };
        let out = process(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            Some(&scorer),
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(!is_deny(&out), "got reason: {:?}", deny_reason(&out));
    }

    #[test]
    fn catastrophic_denies_when_scorer_returns_low_axis() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let scorer = StubScorer {
            result: Ok(SpecChallengeScore::new(0.9, 0.4, 0.9, 0.9, 0.9)), // gaps low
        };
        let out = process(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            Some(&scorer),
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        let reason = deny_reason(&out).unwrap();
        assert!(reason.contains("scorer rejected"));
        assert!(
            reason.contains("gaps=0.40"),
            "should surface per-axis scores; got {reason}"
        );
    }

    #[test]
    fn catastrophic_denies_on_scorer_error() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let scorer = StubScorer {
            result: Err(SpecChallengeScorerError::Backend("rate limit".into())),
        };
        let out = process(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            Some(&scorer),
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(is_deny(&out));
        let reason = deny_reason(&out).unwrap();
        assert!(reason.contains("scorer error"));
        assert!(reason.contains("rate limit"));
    }

    #[test]
    fn strict_blocking_scores_irreversible_too() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let scorer = StubScorer {
            result: Ok(SpecChallengeScore::new(0.5, 0.5, 0.5, 0.5, 0.5)), // all below 0.7
        };
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            Some(&scorer),
            A13EnforcementMode::StrictBlocking,
        );
        assert!(is_deny(&out));
    }

    #[test]
    fn default_blocking_does_not_score_irreversible() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        // Scorer would fail if called; but DefaultBlocking shouldn't call it
        // for Irreversible.
        let scorer = StubScorer {
            result: Err(SpecChallengeScorerError::Backend(
                "should not be called".into(),
            )),
        };
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            Some(&scorer),
            A13EnforcementMode::DefaultBlocking,
        );
        assert!(!is_deny(&out), "got reason: {:?}", deny_reason(&out));
    }

    #[test]
    fn process_with_threshold_uses_override() {
        let challenge = well_formed_challenge();
        let input = input_with_challenge(&challenge);
        let scorer = StubScorer {
            result: Ok(SpecChallengeScore::new(0.6, 0.6, 0.6, 0.6, 0.6)),
        };

        // Threshold 0.5 → passes
        let out = process_with_threshold(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            Some(&scorer),
            A13EnforcementMode::DefaultBlocking,
            0.5,
        );
        assert!(!is_deny(&out));

        // Threshold 0.7 → fails
        let out = process_with_threshold(
            &input,
            ReversibilityClass::Catastrophic,
            None,
            Some(&scorer),
            A13EnforcementMode::DefaultBlocking,
            0.7,
        );
        assert!(is_deny(&out));
    }

    #[test]
    fn observe_only_allows_incomplete_challenge() {
        let mut challenge = well_formed_challenge();
        challenge.gaps = ChallengeCategory::new(vec![]); // silent-empty
        let input = input_with_challenge(&challenge);
        let out = process(
            &input,
            ReversibilityClass::Irreversible,
            None,
            None,
            A13EnforcementMode::ObserveOnly,
        );
        assert!(!is_deny(&out));
    }
}
