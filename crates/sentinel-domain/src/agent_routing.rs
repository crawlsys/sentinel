//! A2 — Agent routing decision artifacts.
//!
//! Per `docs/a2-capability-aware-routing.md` §§3-4. Pure value objects
//! that describe (a) the **outcome** of a routing decision
//! (`RoutingExplanation` — what was considered, what was eliminated,
//! which tie-breakers applied) and (b) the **appraisal data** that
//! feeds the router's tie-breakers (per-agent success/cost/latency
//! over recent calls).
//!
//! No business logic — the router implementation lives behind
//! [`crate::ports::CapabilityRouterPort`] in the infrastructure layer.
//! Lives in its own module rather than the existing `routing.rs` to
//! avoid colliding with sentinel's separate skill-routing regex
//! matcher (the names refer to unrelated concepts).
//!
//! ## R5 quarantine boundary
//!
//! [`AppraisalRecord`]s are *dispatch input*, never *training signal*.
//! The router reads aggregated stats to pick agents on past success;
//! the agents themselves must NOT see appraisal data as feedback. The
//! distinction is load-bearing — using appraisal data as a reward
//! signal is exactly the deception-amplifier loop R5 prohibits.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::capability::{AgentId, CapabilityRequirement};
use crate::dry_run::AuditorVerdict;

// ---------------------------------------------------------------------------
// RequirementSignature — stable hash used as a bucket key for appraisals.
// ---------------------------------------------------------------------------

/// Stable, content-derived hash of a [`CapabilityRequirement`].
///
/// Used as a bucket key in the appraisal store: "how have agents done
/// on requirements *like this one* recently?" Two requirements that
/// differ only in field ordering produce the same signature.
///
/// First 16 hex chars of SHA-256 over a canonicalized JSON encoding
/// (vector entries sorted by their `serde_json::to_string` lexical
/// order — deterministic regardless of construction order).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RequirementSignature(String);

impl RequirementSignature {
    /// Compute the signature from a `CapabilityRequirement`. Sorts each
    /// of `required` / `preferred` / `forbidden` by the lexical order
    /// of their JSON encoding so order-of-construction doesn't shift
    /// the bucket key.
    #[must_use]
    pub fn of(req: &CapabilityRequirement) -> Self {
        let mut required = req.required.clone();
        let mut preferred = req.preferred.clone();
        let mut forbidden = req.forbidden.clone();
        required.sort_by_key(|c| serde_json::to_string(c).unwrap_or_default());
        preferred.sort_by_key(|c| serde_json::to_string(c).unwrap_or_default());
        forbidden.sort_by_key(|c| serde_json::to_string(c).unwrap_or_default());
        let canonical = serde_json::json!({
            "required": required,
            "preferred": preferred,
            "forbidden": forbidden,
        })
        .to_string();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        let digest = hasher.finalize();
        let hex_full = hex::encode(digest);
        Self(hex_full[..16].to_string())
    }

    /// Borrow the underlying 16-char hex hash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequirementSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Routing decision outcome.
// ---------------------------------------------------------------------------

/// Why a candidate agent was eliminated during routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EliminationReason {
    /// One or more `required` capabilities were not satisfied. Lists
    /// the specific unsatisfied items.
    UnsatisfiedRequirement(Vec<UnsatisfiedRequirement>),
    /// A `forbidden` capability was satisfied (vendor ban,
    /// budget shortcut on catastrophic action, etc).
    ForbiddenCapabilityMatched(String),
}

/// A single required capability that an agent failed to satisfy, with
/// a human-readable explanation. Used in [`EliminationReason`] and in
/// [`crate::ports::RoutingError::NoAgentSatisfies`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsatisfiedRequirement {
    /// JSON-rendered capability for stable display + diffing.
    pub capability: String,
    /// Why this agent / this requirement combination failed.
    pub explanation: String,
}

/// Tie-breaker that fired during routing — the router records every
/// step that mattered so `routing explain` is fully traceable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TieBreaker {
    /// Step 2 — preferred-capability score. Higher wins.
    PreferredScore { winner: AgentId, score: usize },
    /// Step 3 — appraisal-based success rate over recent window.
    /// Higher wins. `cohort_size` is the number of records used.
    AppraisalSuccessRate {
        winner: AgentId,
        success_rate: f32,
        cohort_size: u32,
    },
    /// Step 4 — cost. Cheapest cost-per-input-token wins. Used when
    /// the requirement doesn't already disqualify on cost.
    Cost {
        winner: AgentId,
        cost_per_input_token: f32,
    },
    /// Step 5 — latency. Fastest typical-latency wins.
    Latency { winner: AgentId, typical_latency_ms: u32 },
    /// Step 6 — final deterministic fallback: lexical `AgentId` order.
    StableId { winner: AgentId },
}

/// Full decision tree for a single routing call. Returned by
/// [`crate::ports::CapabilityRouterPort::explain`] (and `sentinel
/// routing explain` CLI subcommand once Phase 3b ships it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingExplanation {
    /// The agent the router chose, or `None` when no agent
    /// satisfied the requirements.
    pub chosen: Option<AgentId>,
    /// Agents that passed `required` + `forbidden` filtering and
    /// became tie-breaker candidates.
    pub candidates: Vec<AgentId>,
    /// Agents that were eliminated, with the reason.
    pub eliminated: Vec<(AgentId, EliminationReason)>,
    /// Tie-breaker steps that fired, in order (deterministic).
    pub tie_breakers_applied: Vec<TieBreaker>,
    /// Hash of the requirement that drove this decision — useful for
    /// correlating across explain calls + appraisal records.
    pub requirement_signature: RequirementSignature,
}

// ---------------------------------------------------------------------------
// Appraisal records — past-decision outcome capture.
// ---------------------------------------------------------------------------

/// Coarse outcome label for a completed work item. Stored in
/// [`AppraisalRecord`]s; aggregated into success-rate signals for the
/// router's tie-breaker step 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppraisalOutcome {
    /// Work completed and verified — auditor passed (A3) / critic
    /// passed (BA5) / no downstream gate blocked.
    Success,
    /// Work completed but at least one downstream signal flagged a
    /// concern (low-confidence Pass, partial axes failure, etc).
    PartialSuccess,
    /// Auditor blocked, critic flagged a stop-ship, or runtime error.
    Failure,
    /// Work aborted before completion (timeout, operator cancel,
    /// session ended).
    Abandoned,
}

/// One past dispatch outcome. The appraisal store accumulates these
/// per-session; aggregation queries (success-rate, mean-cost,
/// mean-latency) feed the router's tie-breakers.
///
/// **R5 boundary**: these records are dispatch input only. Agents
/// must NOT see them as training feedback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppraisalRecord {
    /// Agent that handled the work.
    pub agent_id: AgentId,
    /// Bucket key — which requirement was this?
    pub requirement_signature: RequirementSignature,
    /// Outcome label.
    pub outcome: AppraisalOutcome,
    /// Auditor verdict if A3 was in scope. Optional — not every work
    /// item routes through A3.
    pub auditor_signal: Option<AuditorVerdict>,
    /// Observed cost (USD) — actuals, not the budget.
    pub actual_cost_usd: f32,
    /// Observed wall-clock latency (ms).
    pub actual_latency_ms: u32,
    /// Token counts (operator-visible billing reconciliation).
    pub tokens_in: u32,
    pub tokens_out: u32,
    /// When this record was created.
    pub timestamp: DateTime<Utc>,
}

/// Aggregate stats for an agent on a specific requirement signature
/// over a window. Returned by
/// [`crate::ports::AppraisalStorePort::aggregate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AggregateStats {
    /// Number of records in the window.
    pub cohort_size: u32,
    /// Fraction of `cohort_size` with `Success` outcome. `[0.0, 1.0]`.
    pub success_rate: f32,
    /// Fraction with `Success` or `PartialSuccess`. `[0.0, 1.0]`.
    pub permissive_success_rate: f32,
    /// Arithmetic mean of `actual_cost_usd` over the window.
    pub mean_cost_usd: f32,
    /// Arithmetic mean of `actual_latency_ms` over the window.
    pub mean_latency_ms: f32,
}

impl AggregateStats {
    /// An empty aggregate — no records yet for this (agent, signature)
    /// bucket. Used by the router to fall through to the next
    /// tie-breaker when appraisal data is absent.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            cohort_size: 0,
            success_rate: 0.0,
            permissive_success_rate: 0.0,
            mean_cost_usd: 0.0,
            mean_latency_ms: 0.0,
        }
    }

    /// Returns `true` iff this aggregate has any records to base a
    /// tie-breaker on.
    #[must_use]
    pub const fn has_data(&self) -> bool {
        self.cohort_size > 0
    }

    /// Compute aggregates from a slice of records. Used by the
    /// infrastructure adapter and by unit tests of the router.
    #[must_use]
    pub fn from_records(records: &[AppraisalRecord]) -> Self {
        let cohort_size = u32::try_from(records.len()).unwrap_or(u32::MAX);
        if cohort_size == 0 {
            return Self::empty();
        }
        let n = f32::from(u16::try_from(cohort_size.min(u32::from(u16::MAX))).unwrap_or(u16::MAX));
        let mut successes = 0u32;
        let mut permissive_successes = 0u32;
        let mut cost_sum = 0.0_f32;
        let mut latency_sum = 0.0_f32;
        for r in records {
            match r.outcome {
                AppraisalOutcome::Success => {
                    successes += 1;
                    permissive_successes += 1;
                }
                AppraisalOutcome::PartialSuccess => {
                    permissive_successes += 1;
                }
                _ => {}
            }
            cost_sum += r.actual_cost_usd;
            #[allow(clippy::cast_precision_loss)]
            {
                latency_sum += r.actual_latency_ms as f32;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let success_rate = successes as f32 / n;
        #[allow(clippy::cast_precision_loss)]
        let permissive_success_rate = permissive_successes as f32 / n;
        Self {
            cohort_size,
            success_rate,
            permissive_success_rate,
            mean_cost_usd: cost_sum / n,
            mean_latency_ms: latency_sum / n,
        }
    }
}

// ---------------------------------------------------------------------------
// Window — time-bounded query argument for AppraisalStorePort::aggregate.
// ---------------------------------------------------------------------------

/// Time / count window for an appraisal aggregation query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppraisalWindow {
    /// Most recent `n` records (regardless of age).
    LastN(u32),
    /// All records within the last `hours` of `now()`.
    LastHours(u32),
    /// All records (no bound — operator chose to weight everything).
    All,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, ReasoningLevel, SchemaRef, VendorClass};
    use crate::dry_run::{AuditorAxes, AuditorDecision};

    fn agent(s: &str) -> AgentId {
        AgentId::new(s).unwrap()
    }

    fn fixture_record(agent: &AgentId, outcome: AppraisalOutcome, cost: f32) -> AppraisalRecord {
        AppraisalRecord {
            agent_id: agent.clone(),
            requirement_signature: RequirementSignature("deadbeefcafebabe".to_string()),
            outcome,
            auditor_signal: None,
            actual_cost_usd: cost,
            actual_latency_ms: 5000,
            tokens_in: 1000,
            tokens_out: 200,
            timestamp: Utc::now(),
        }
    }

    // ---- RequirementSignature ----

    #[test]
    fn signature_stable_across_construction_orders() {
        let r1 = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Standard),
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
        ]);
        let r2 = CapabilityRequirement::new(vec![
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
            Capability::Reasoning(ReasoningLevel::Standard),
        ]);
        assert_eq!(
            RequirementSignature::of(&r1),
            RequirementSignature::of(&r2),
            "order of required capabilities should not change the signature"
        );
    }

    #[test]
    fn signature_changes_when_requirements_differ() {
        let r1 = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let r2 = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Deep)]);
        assert_ne!(
            RequirementSignature::of(&r1),
            RequirementSignature::of(&r2),
            "different required levels should produce distinct signatures"
        );
    }

    #[test]
    fn signature_changes_when_preferred_differs() {
        let base = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let with_pref = base
            .clone()
            .with_preferred(Capability::StructuredOutput(SchemaRef::AuditorVerdict));
        assert_ne!(
            RequirementSignature::of(&base),
            RequirementSignature::of(&with_pref),
            "preferred capabilities affect the signature"
        );
    }

    #[test]
    fn signature_is_16_hex_chars() {
        let r = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let sig = RequirementSignature::of(&r);
        assert_eq!(sig.as_str().len(), 16);
        assert!(sig.as_str().chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ---- AggregateStats::from_records ----

    #[test]
    fn aggregate_empty_returns_no_data() {
        let agg = AggregateStats::from_records(&[]);
        assert!(!agg.has_data());
        assert_eq!(agg.cohort_size, 0);
    }

    #[test]
    fn aggregate_counts_successes_strict_vs_permissive() {
        let a = agent("kimi");
        let records = vec![
            fixture_record(&a, AppraisalOutcome::Success, 0.01),
            fixture_record(&a, AppraisalOutcome::Success, 0.02),
            fixture_record(&a, AppraisalOutcome::PartialSuccess, 0.015),
            fixture_record(&a, AppraisalOutcome::Failure, 0.025),
        ];
        let agg = AggregateStats::from_records(&records);
        assert_eq!(agg.cohort_size, 4);
        assert!((agg.success_rate - 0.5).abs() < 1e-5, "2 of 4 strict successes");
        assert!(
            (agg.permissive_success_rate - 0.75).abs() < 1e-5,
            "3 of 4 (S + PS) permissive successes"
        );
    }

    #[test]
    fn aggregate_computes_mean_cost_and_latency() {
        let a = agent("opus");
        let mut r1 = fixture_record(&a, AppraisalOutcome::Success, 0.10);
        r1.actual_latency_ms = 4000;
        let mut r2 = fixture_record(&a, AppraisalOutcome::Success, 0.30);
        r2.actual_latency_ms = 8000;
        let agg = AggregateStats::from_records(&[r1, r2]);
        assert!((agg.mean_cost_usd - 0.20).abs() < 1e-5);
        assert!((agg.mean_latency_ms - 6000.0).abs() < 1e-5);
    }

    #[test]
    fn aggregate_abandoned_counts_as_non_success_in_both() {
        let a = agent("kimi");
        let records = vec![
            fixture_record(&a, AppraisalOutcome::Abandoned, 0.0),
            fixture_record(&a, AppraisalOutcome::Success, 0.02),
        ];
        let agg = AggregateStats::from_records(&records);
        assert!((agg.success_rate - 0.5).abs() < 1e-5);
        assert!((agg.permissive_success_rate - 0.5).abs() < 1e-5, "Abandoned is not partial");
    }

    // ---- AppraisalRecord serde ----

    #[test]
    fn appraisal_record_roundtrips_through_json() {
        let a = agent("kimi");
        let r = AppraisalRecord {
            agent_id: a,
            requirement_signature: RequirementSignature("deadbeef00000000".to_string()),
            outcome: AppraisalOutcome::PartialSuccess,
            auditor_signal: Some(AuditorVerdict {
                decision: AuditorDecision::Pass,
                confidence: 0.7,
                axes: AuditorAxes::new(0.8, 0.7, 0.75, 0.6),
                reasoning: "borderline".to_string(),
                auditor_model: "ollama-cloud:kimi-k2.6".to_string(),
            }),
            actual_cost_usd: 0.012,
            actual_latency_ms: 4200,
            tokens_in: 1500,
            tokens_out: 320,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let parsed: AppraisalRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(r.agent_id, parsed.agent_id);
        assert_eq!(r.requirement_signature, parsed.requirement_signature);
        assert_eq!(r.outcome, parsed.outcome);
        assert!((r.actual_cost_usd - parsed.actual_cost_usd).abs() < 1e-5);
    }

    // ---- RoutingExplanation serde ----

    #[test]
    fn routing_explanation_roundtrips_through_json() {
        let r = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let expl = RoutingExplanation {
            chosen: Some(agent("kimi")),
            candidates: vec![agent("kimi"), agent("opus")],
            eliminated: vec![(
                agent("opus"),
                EliminationReason::ForbiddenCapabilityMatched(
                    "Vendor(Anthropic) forbidden".to_string(),
                ),
            )],
            tie_breakers_applied: vec![TieBreaker::PreferredScore {
                winner: agent("kimi"),
                score: 2,
            }],
            requirement_signature: RequirementSignature::of(&r),
        };
        let json = serde_json::to_string(&expl).unwrap();
        let parsed: RoutingExplanation = serde_json::from_str(&json).unwrap();
        assert_eq!(expl, parsed);
    }

    // ---- Send + Sync ----

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RequirementSignature>();
        assert_send_sync::<AppraisalRecord>();
        assert_send_sync::<AggregateStats>();
        assert_send_sync::<RoutingExplanation>();
        assert_send_sync::<AppraisalWindow>();
    }
}
