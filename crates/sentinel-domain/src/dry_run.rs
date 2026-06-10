//! A3 — Dry-run-then-commit value objects.
//!
//! Per the A3 design (`docs/a3-dry-run-then-commit.md`), every irreversible
//! or catastrophic mutating action emits a structured *dry-run* artifact
//! describing the agent's intent, reasoning, and expected effect *before*
//! the action commits. A trusted auditor (a separate-model-family LLM via
//! MCP per A2's vendor-class routing) scores the dry-run; the action only
//! commits on auditor pass.
//!
//! This module defines the pure-data shapes the hook + adapter layers
//! operate against:
//!
//! - [`DryRunRequest`] — the artifact the acting agent emits.
//! - [`AuditorVerdict`] — the structured response the auditor returns,
//!   including a [`AuditorDecision`] (pass/block) and per-axis scores.
//! - [`AuditorAxes`] — the four axes the auditor scores: `correctness`,
//!   `intent_alignment`, `safety`, `unstated_assumptions`.
//! - [`AuditorError`] — the typed failure modes the auditor adapter can
//!   surface (unavailable, timed out, malformed dry-run, vendor error).
//!
//! The trait [`AuditorPort`](crate::ports::AuditorPort) is defined alongside
//! the other sentinel-domain ports in `ports.rs`.
//!
//! Subsequent phases add: hook (`sentinel-application`), in-memory and
//! real adapters (`sentinel-infrastructure`), wiring into `hook_cmd.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ReversibilityClass;

// ---------------------------------------------------------------------------
// DryRunRequest — the artifact the acting agent emits before any irreversible
// commit.
// ---------------------------------------------------------------------------

/// Structured dry-run artifact produced by the acting agent before an
/// `Irreversible` or `Catastrophic` action commits.
///
/// Routed through [`AuditorPort::score`](crate::ports::AuditorPort::score) to
/// an LLM auditor (from a different vendor than the acting agent per A2's
/// vendor-class routing). The auditor reads `agent_intent`, `agent_reasoning`,
/// and `agent_expected_effect` alongside the proposed `tool_input`, scores
/// the artifact on four axes, and returns an [`AuditorVerdict`].
///
/// The acting agent is responsible for populating the prose fields
/// (`agent_intent`, `agent_reasoning`, `agent_expected_effect`). Sentinel's
/// hook constructs the structural fields (`session_id`, `tool_name`,
/// `tool_input`, `reversibility_class`, `constructed_at`) from the
/// `PreToolUse` input + the A6 classifier output. Future tool-input
/// conventions may let agents populate the prose fields inline (e.g.
/// `_intent` / `_reasoning` / `_expected_effect` JSON fields on mutating
/// tool calls); until then the agent supplies them via transcript context
/// that the hook extracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunRequest {
    /// Sentinel session identifier (used for proof-chain attribution).
    pub session_id: String,
    /// The tool the acting agent proposes to call (e.g., `Bash`,
    /// `mcp__gmail__send_message`).
    pub tool_name: String,
    /// The structured tool input the agent intends to pass.
    pub tool_input: serde_json::Value,
    /// Free-text: what the agent intends to accomplish by this action.
    pub agent_intent: String,
    /// Free-text: why the agent believes this action accomplishes the
    /// intent.
    pub agent_reasoning: String,
    /// Free-text: what the agent expects to be true after the action
    /// commits (the auditor verifies the expected effect against the
    /// proposed `tool_input`).
    pub agent_expected_effect: String,
    /// Reversibility class assigned by the A6 classifier — informs the
    /// auditor about blast radius without re-running the classifier.
    pub reversibility_class: ReversibilityClass,
    /// Wall-clock timestamp at which the hook constructed this artifact
    /// (used for proof-chain ordering + auditor freshness checks).
    pub constructed_at: DateTime<Utc>,
}

impl DryRunRequest {
    /// Construct a new dry-run request with required structural fields.
    /// Prose fields default to empty strings — callers populate them
    /// before submitting to the auditor.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        reversibility_class: ReversibilityClass,
        constructed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            tool_name: tool_name.into(),
            tool_input,
            agent_intent: String::new(),
            agent_reasoning: String::new(),
            agent_expected_effect: String::new(),
            reversibility_class,
            constructed_at,
        }
    }

    /// Builder-style setter for `agent_intent`.
    #[must_use]
    pub fn with_intent(mut self, intent: impl Into<String>) -> Self {
        self.agent_intent = intent.into();
        self
    }

    /// Builder-style setter for `agent_reasoning`.
    #[must_use]
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Self {
        self.agent_reasoning = reasoning.into();
        self
    }

    /// Builder-style setter for `agent_expected_effect`.
    #[must_use]
    pub fn with_expected_effect(mut self, expected_effect: impl Into<String>) -> Self {
        self.agent_expected_effect = expected_effect.into();
        self
    }

    /// `true` when all three prose fields (`agent_intent`,
    /// `agent_reasoning`, `agent_expected_effect`) are non-empty. An
    /// incomplete dry-run is generally not worth auditor cycles — the
    /// hook can short-circuit to block before paying for the LLM call.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.agent_intent.trim().is_empty()
            && !self.agent_reasoning.trim().is_empty()
            && !self.agent_expected_effect.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// AuditorVerdict + AuditorDecision + AuditorAxes — the auditor's response.
// ---------------------------------------------------------------------------

/// Structured response from the auditor for a dry-run.
///
/// Carries the binary commit/block [`AuditorDecision`], a `confidence`
/// score (used by the hook's human-sampling escalation: anything below
/// the configured threshold escalates regardless of decision), per-axis
/// scores in [`AuditorAxes`], free-text `reasoning` the operator sees
/// when the action blocks, and the `auditor_model` identifier (used by
/// proof-chain audit + per-model appraisal counters per A2's routing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditorVerdict {
    /// Pass or Block (with reason).
    pub decision: AuditorDecision,
    /// Auditor's self-reported confidence (0.0–1.0). Below the configured
    /// human-sample threshold (default 0.85) the hook escalates to human
    /// review regardless of decision.
    pub confidence: f32,
    /// Per-axis sub-scores (`correctness`, `intent_alignment`, `safety`,
    /// `unstated_assumptions`). Each 0.0–1.0.
    pub axes: AuditorAxes,
    /// Free-text reasoning the operator sees when the action is blocked
    /// or escalates to human review.
    pub reasoning: String,
    /// Identifier of the model that produced this verdict. Vendor-prefix
    /// convention: `"anthropic:claude-opus-4-7"`, `"openai:gpt-5.5"`,
    /// `"google:gemini-3.1-pro"`, `"ollama:kimi-k2-6"`, etc.
    pub auditor_model: String,
}

/// Binary outcome from the auditor's review of a dry-run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "PascalCase")]
pub enum AuditorDecision {
    /// Auditor passed the dry-run — action may commit (subject to the
    /// hook's separate human-sampling escalation for catastrophic-class
    /// or low-confidence verdicts).
    Pass,
    /// Auditor blocked the dry-run with a free-text reason that surfaces
    /// to the operator.
    Block {
        /// Operator-facing explanation for the block. Should be specific
        /// enough that the operator can either fix the proposed action,
        /// challenge the auditor's reasoning, or invoke the
        /// `hygiene_override`.
        reason: String,
    },
}

impl AuditorDecision {
    /// Convenience: true iff the auditor returned `Pass`.
    #[must_use]
    pub const fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Convenience: true iff the auditor returned `Block`.
    #[must_use]
    pub const fn is_block(&self) -> bool {
        matches!(self, Self::Block { .. })
    }
}

/// Per-axis sub-scores from the auditor. Each axis is 0.0–1.0; higher is better.
///
/// A composite verdict can be derived from these — currently the auditor
/// reports the composite as `confidence` on [`AuditorVerdict`]; per-axis
/// scores stay available for operator-facing diagnostics + future
/// debate-between-auditors arbitration per A4.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AuditorAxes {
    /// Does the proposed action actually achieve the stated intent?
    pub correctness: f32,
    /// Does the stated intent match recent transcript context (no
    /// goal-drift between what the user asked and what the agent
    /// articulated)?
    pub intent_alignment: f32,
    /// Does the proposed action introduce known-bad patterns (data
    /// exfiltration, escape-from-context, etc.)?
    pub safety: f32,
    /// Does the dry-run rely on facts the auditor cannot verify from the
    /// transcript / source materials?
    pub unstated_assumptions: f32,
}

impl AuditorAxes {
    /// Construct, clamping each axis into the valid 0.0–1.0 range.
    /// Out-of-range scores from an adapter (e.g., LLM returning `-0.2`
    /// or `1.5`) are quietly clamped — the auditor's output is treated
    /// as advisory data, not as a contract over the precise number.
    #[must_use]
    pub fn new(
        correctness: f32,
        intent_alignment: f32,
        safety: f32,
        unstated_assumptions: f32,
    ) -> Self {
        Self {
            correctness: clamp01(correctness),
            intent_alignment: clamp01(intent_alignment),
            safety: clamp01(safety),
            unstated_assumptions: clamp01(unstated_assumptions),
        }
    }

    /// Mean of the four axes — a quick composite for sorting / dashboards.
    /// The hook uses [`AuditorVerdict::confidence`] for human-sampling
    /// decisions, not this mean; provided for operator diagnostics.
    #[must_use]
    pub fn mean(&self) -> f32 {
        (self.correctness + self.intent_alignment + self.safety + self.unstated_assumptions) / 4.0
    }

    /// Returns the lowest axis score and its name. Useful for surfacing
    /// "the auditor flagged X axis" to the operator when a borderline
    /// verdict needs human review.
    #[must_use]
    pub fn weakest_axis(&self) -> (&'static str, f32) {
        let mut weakest = ("correctness", self.correctness);
        for candidate in [
            ("intent_alignment", self.intent_alignment),
            ("safety", self.safety),
            ("unstated_assumptions", self.unstated_assumptions),
        ] {
            if candidate.1 < weakest.1 {
                weakest = candidate;
            }
        }
        weakest
    }
}

#[inline]
const fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// AuditorError — failure modes the adapter can surface.
// ---------------------------------------------------------------------------

/// Typed failure modes from an [`AuditorPort`](crate::ports::AuditorPort)
/// adapter. The hook's policy on each failure depends on the action's
/// reversibility class:
///
/// - For `Irreversible`: any `AuditorError` blocks the action; the
///   operator can retry once the auditor is reachable.
/// - For `Catastrophic`: any `AuditorError` blocks AND escalates to
///   human review (the auditor being down for a catastrophic action is
///   exactly when human review matters most).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditorError {
    /// The auditor's MCP endpoint / vendor API is unreachable.
    Unavailable(String),
    /// The auditor took longer than the configured timeout to respond.
    /// The hook treats this as a block per the policy above.
    TimedOut(std::time::Duration),
    /// The dry-run artifact was malformed (e.g., empty prose fields when
    /// the adapter expects them populated). The adapter can choose to
    /// return this synchronously rather than calling out to the model.
    InvalidDryRun(String),
    /// The vendor returned an unparseable response (e.g., not valid JSON
    /// when the adapter expected structured output).
    MalformedResponse(String),
    /// Any other adapter-specific error wrapped for surface stability.
    Other(String),
}

impl std::fmt::Display for AuditorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(msg) => write!(f, "auditor unavailable: {msg}"),
            Self::TimedOut(dur) => write!(f, "auditor timed out after {dur:?}"),
            Self::InvalidDryRun(msg) => write!(f, "invalid dry-run: {msg}"),
            Self::MalformedResponse(msg) => {
                write!(f, "vendor returned malformed response: {msg}")
            }
            Self::Other(msg) => write!(f, "auditor error: {msg}"),
        }
    }
}

impl std::error::Error for AuditorError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-05-17T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    // ---- DryRunRequest ----

    #[test]
    fn dry_run_request_new_defaults_prose_fields_to_empty() {
        let req = DryRunRequest::new(
            "sess-1",
            "Edit",
            serde_json::json!({}),
            ReversibilityClass::ReversibleWithEffort,
            fixed_time(),
        );
        assert_eq!(req.agent_intent, "");
        assert_eq!(req.agent_reasoning, "");
        assert_eq!(req.agent_expected_effect, "");
        assert!(!req.is_complete());
    }

    #[test]
    fn dry_run_request_builder_populates_prose_fields() {
        let req = DryRunRequest::new(
            "sess-1",
            "Edit",
            serde_json::json!({}),
            ReversibilityClass::Irreversible,
            fixed_time(),
        )
        .with_intent("ship a security fix")
        .with_reasoning("CVE-2026-... requires patching the auth path")
        .with_expected_effect("auth requests use the new validator");
        assert!(req.is_complete());
        assert_eq!(req.agent_intent, "ship a security fix");
    }

    #[test]
    fn is_complete_requires_all_three_prose_fields_nonblank() {
        let base = DryRunRequest::new(
            "sess-1",
            "Edit",
            serde_json::json!({}),
            ReversibilityClass::Irreversible,
            fixed_time(),
        );
        // Any one missing → not complete.
        assert!(!base
            .clone()
            .with_intent("x")
            .with_reasoning("y")
            .is_complete());
        assert!(!base
            .clone()
            .with_reasoning("y")
            .with_expected_effect("z")
            .is_complete());
        assert!(!base
            .clone()
            .with_intent("x")
            .with_expected_effect("z")
            .is_complete());
        // Whitespace-only doesn't count.
        assert!(!base
            .clone()
            .with_intent("   ")
            .with_reasoning("y")
            .with_expected_effect("z")
            .is_complete());
        // All three nonblank → complete.
        assert!(base
            .with_intent("x")
            .with_reasoning("y")
            .with_expected_effect("z")
            .is_complete());
    }

    #[test]
    fn dry_run_request_serde_roundtrip() {
        let original = DryRunRequest::new(
            "sess-abc",
            "mcp__gmail__send_message",
            serde_json::json!({"to": "ceo@example.com", "subject": "test"}),
            ReversibilityClass::Catastrophic,
            fixed_time(),
        )
        .with_intent("send weekly digest")
        .with_reasoning("scheduled digest cron triggered")
        .with_expected_effect("CEO receives email; no other recipients touched");
        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: DryRunRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }

    // ---- AuditorAxes ----

    #[test]
    fn axes_new_clamps_out_of_range_values() {
        let axes = AuditorAxes::new(-0.5, 1.5, 0.7, 0.0);
        assert!((axes.correctness - 0.0).abs() < f32::EPSILON);
        assert!((axes.intent_alignment - 1.0).abs() < f32::EPSILON);
        assert!((axes.safety - 0.7).abs() < f32::EPSILON);
        assert!((axes.unstated_assumptions - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn axes_mean_computes_arithmetic_mean() {
        let axes = AuditorAxes::new(1.0, 1.0, 0.0, 0.0);
        assert!((axes.mean() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn axes_weakest_axis_finds_lowest_score() {
        let axes = AuditorAxes::new(0.9, 0.8, 0.3, 0.6);
        let (name, score) = axes.weakest_axis();
        assert_eq!(name, "safety");
        assert!((score - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn axes_weakest_axis_ties_pick_first_in_declaration_order() {
        let axes = AuditorAxes::new(0.5, 0.5, 0.5, 0.5);
        // correctness comes first
        assert_eq!(axes.weakest_axis().0, "correctness");
    }

    #[test]
    fn axes_serde_roundtrip() {
        let axes = AuditorAxes::new(0.9, 0.8, 0.7, 0.6);
        let json = serde_json::to_string(&axes).unwrap();
        let parsed: AuditorAxes = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, axes);
    }

    // ---- AuditorDecision ----

    #[test]
    fn decision_is_pass_predicate() {
        assert!(AuditorDecision::Pass.is_pass());
        assert!(!AuditorDecision::Block { reason: "x".into() }.is_pass());
    }

    #[test]
    fn decision_is_block_predicate() {
        assert!(!AuditorDecision::Pass.is_block());
        assert!(AuditorDecision::Block { reason: "x".into() }.is_block());
    }

    #[test]
    fn decision_serde_roundtrip_pass() {
        let d = AuditorDecision::Pass;
        let json = serde_json::to_string(&d).unwrap();
        // tagged enum: { "kind": "Pass" }
        assert!(json.contains("\"Pass\""));
        let parsed: AuditorDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, d);
    }

    #[test]
    fn decision_serde_roundtrip_block() {
        let d = AuditorDecision::Block {
            reason: "exfiltration risk".into(),
        };
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"Block\""));
        assert!(json.contains("exfiltration risk"));
        let parsed: AuditorDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, d);
    }

    // ---- AuditorVerdict ----

    #[test]
    fn verdict_serde_roundtrip() {
        let v = AuditorVerdict {
            decision: AuditorDecision::Pass,
            confidence: 0.92,
            axes: AuditorAxes::new(0.95, 0.9, 0.95, 0.88),
            reasoning: "no concerns; intent matches transcript".into(),
            auditor_model: "openai:gpt-5.5".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let parsed: AuditorVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, v);
    }

    // ---- AuditorError ----

    #[test]
    fn error_display_shapes_are_useful() {
        let e = AuditorError::Unavailable("connection refused".into());
        assert!(format!("{e}").contains("auditor unavailable"));
        assert!(format!("{e}").contains("connection refused"));

        let e = AuditorError::TimedOut(std::time::Duration::from_secs(5));
        assert!(format!("{e}").contains("timed out"));

        let e = AuditorError::InvalidDryRun("empty intent".into());
        assert!(format!("{e}").contains("invalid dry-run"));

        let e = AuditorError::MalformedResponse("not JSON".into());
        assert!(format!("{e}").contains("malformed response"));

        let e = AuditorError::Other("vendor unknown error".into());
        assert!(format!("{e}").contains("auditor error"));
    }

    #[test]
    fn error_implements_std_error() {
        // Compile-time assert: AuditorError satisfies std::error::Error.
        fn assert_error<T: std::error::Error>() {}
        assert_error::<AuditorError>();
    }

    #[test]
    fn types_are_send_sync() {
        // Compile-time assertions: the value objects can flow across
        // task boundaries (required for any async adapter to use them).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DryRunRequest>();
        assert_send_sync::<AuditorVerdict>();
        assert_send_sync::<AuditorDecision>();
        assert_send_sync::<AuditorAxes>();
        assert_send_sync::<AuditorError>();
    }
}
