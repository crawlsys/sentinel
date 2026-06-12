//! A2 — Capability router (pure algorithm + test helpers).
//!
//! Per `docs/a2-capability-aware-routing.md` §3. Implements the
//! deterministic 6-step tie-breaker chain that picks an [`AgentId`]
//! from a set of [`AgentCapabilityProfile`]s given a
//! [`CapabilityRequirement`].
//!
//! ## Layering
//!
//! - [`pick`] — pure free function that runs the algorithm against
//!   any slice of profiles + optional [`AppraisalStorePort`].
//!   Returns a full [`RoutingExplanation`] (chosen, candidates,
//!   eliminated-with-reasons, fired tie-breakers). No IO; this is
//!   the reference behavior consumed by every router adapter.
//! - [`StaticCapabilityRouter`] — application-layer test helper.
//!   Wraps a static profile list (and optional in-memory appraisal
//!   store); implements [`CapabilityRouterPort`]. Equivalent role to
//!   [`StaticReversibilityClassifier`](crate::reversibility_classifier::StaticReversibilityClassifier)
//!   for the A6 substrate.
//! - The TOML-backed production router (Phase 3b in
//!   `sentinel-infrastructure`) wraps the same [`pick`] function
//!   around config-loaded profiles. The algorithm lives here so
//!   both adapters share the same behavior.
//!
//! ## Tie-breaker chain (in order)
//!
//! 1. Required + forbidden filtering — agents that fail this step
//!    are eliminated with [`EliminationReason::UnsatisfiedRequirement`]
//!    or [`EliminationReason::ForbiddenCapabilityMatched`].
//! 2. Preferred score — most preferred-capabilities satisfied wins.
//! 3. Appraisal success rate — when the store is configured and has
//!    data for `(agent, requirement_signature)`, higher recent
//!    success rate wins.
//! 4. Cost — cheapest `cost_per_input_token` wins.
//! 5. Latency — fastest `typical_latency_ms` wins.
//! 6. Stable `AgentId` ordering — final deterministic fallback.

use sentinel_domain::agent_routing::{
    AppraisalWindow, EliminationReason, RequirementSignature, RoutingExplanation, TieBreaker,
    UnsatisfiedRequirement,
};
use sentinel_domain::capability::{
    AgentCapabilityProfile, AgentId, Capability, CapabilityRequirement,
};
use sentinel_domain::ports::{AppraisalStorePort, CapabilityRouterPort, RoutingError};

// ---------------------------------------------------------------------------
// pick — the pure algorithm.
// ---------------------------------------------------------------------------

/// Default appraisal window used by [`pick`] when the router doesn't
/// override it.
///
/// Last 200 records is the spec's calibration target — large enough
/// to be statistically meaningful, small enough to react to recent
/// drift.
pub const DEFAULT_APPRAISAL_WINDOW: AppraisalWindow = AppraisalWindow::LastN(200);

/// Minimum cohort size before appraisal-rate tie-breaking is allowed
/// to fire.
///
/// Below this, the cohort is too small to be a reliable signal and the
/// chain falls through to cost/latency. The threshold is conservative
/// on purpose — small cohorts of "100% success in 3 calls" would
/// otherwise dominate well-calibrated cohorts of "92% in 200 calls"
/// via pure rate comparison.
pub const MIN_COHORT_FOR_APPRAISAL_TIE_BREAK: u32 = 20;

/// Run the routing algorithm. Pure function, no IO. Returns a full
/// [`RoutingExplanation`] so every caller can inspect why the chosen
/// agent won — even on test fixtures.
///
/// `appraisal_store: None` skips step 3 entirely (cost moves up to
/// step 3 effectively). Operators can also disable appraisal-based
/// tie-breaking via `config/routing-policy.toml` (Phase 3b) for the
/// same outcome.
#[allow(clippy::too_many_lines)] // the 6-step tie-breaker chain is more readable inline than split across helpers
pub fn pick(
    profiles: &[AgentCapabilityProfile],
    requirement: &CapabilityRequirement,
    appraisal_store: Option<&dyn AppraisalStorePort>,
    window: AppraisalWindow,
) -> RoutingExplanation {
    let signature = RequirementSignature::of(requirement);

    // Step 1 — required + forbidden filtering.
    let mut candidates: Vec<&AgentCapabilityProfile> = Vec::new();
    let mut eliminated: Vec<(AgentId, EliminationReason)> = Vec::new();
    for profile in profiles {
        let mut unsatisfied: Vec<UnsatisfiedRequirement> = Vec::new();
        for req_cap in &requirement.required {
            if !req_cap.is_satisfied_by(profile) {
                unsatisfied.push(UnsatisfiedRequirement {
                    capability: serde_json::to_string(req_cap).unwrap_or_default(),
                    explanation: format!(
                        "agent {} does not satisfy required capability",
                        profile.agent_id
                    ),
                });
            }
        }
        if let Some(forbidden_match) = requirement
            .forbidden
            .iter()
            .find(|c| c.is_satisfied_by(profile))
        {
            let cap_json = serde_json::to_string(forbidden_match).unwrap_or_default();
            eliminated.push((
                profile.agent_id.clone(),
                EliminationReason::ForbiddenCapabilityMatched(cap_json),
            ));
            continue;
        }
        if unsatisfied.is_empty() {
            candidates.push(profile);
        } else {
            eliminated.push((
                profile.agent_id.clone(),
                EliminationReason::UnsatisfiedRequirement(unsatisfied),
            ));
        }
    }

    let candidate_ids: Vec<AgentId> = candidates.iter().map(|p| p.agent_id.clone()).collect();
    let mut tie_breakers: Vec<TieBreaker> = Vec::new();

    if candidates.is_empty() {
        return RoutingExplanation {
            chosen: None,
            candidates: candidate_ids,
            eliminated,
            tie_breakers_applied: tie_breakers,
            requirement_signature: signature,
        };
    }

    // Step 2 — preferred score (higher wins).
    let max_pref = candidates
        .iter()
        .map(|p| requirement.preferred_score(p))
        .max()
        .unwrap_or(0);
    if !requirement.preferred.is_empty() {
        let pre = candidates.len();
        candidates.retain(|p| requirement.preferred_score(p) == max_pref);
        if candidates.len() < pre {
            if let Some(winner) = candidates.first() {
                tie_breakers.push(TieBreaker::PreferredScore {
                    winner: winner.agent_id.clone(),
                    score: max_pref,
                });
            }
        }
    }
    if candidates.len() == 1 {
        return finalize(
            candidates[0],
            candidate_ids,
            eliminated,
            tie_breakers,
            signature,
        );
    }

    // Step 3 — appraisal success rate (higher wins). Skipped when no
    // store is configured or no cohort clears the minimum size.
    if let Some(store) = appraisal_store {
        let mut best_rate: f32 = -1.0;
        let mut best_cohort: u32 = 0;
        let mut survivor: Vec<&AgentCapabilityProfile> = Vec::new();
        for p in &candidates {
            let agg = store.aggregate(&p.agent_id, &signature, window);
            if agg.cohort_size < MIN_COHORT_FOR_APPRAISAL_TIE_BREAK {
                continue;
            }
            // Strict > so when ties remain we fall through to cost.
            if agg.success_rate > best_rate {
                best_rate = agg.success_rate;
                best_cohort = agg.cohort_size;
                survivor.clear();
                survivor.push(p);
            } else if (agg.success_rate - best_rate).abs() < f32::EPSILON {
                survivor.push(p);
            }
        }
        if !survivor.is_empty() && survivor.len() < candidates.len() {
            candidates = survivor;
            if let Some(winner) = candidates.first() {
                tie_breakers.push(TieBreaker::AppraisalSuccessRate {
                    winner: winner.agent_id.clone(),
                    success_rate: best_rate,
                    cohort_size: best_cohort,
                });
            }
        }
    }
    if candidates.len() == 1 {
        return finalize(
            candidates[0],
            candidate_ids,
            eliminated,
            tie_breakers,
            signature,
        );
    }

    // Step 4 — cost (cheapest wins).
    let min_cost = candidates
        .iter()
        .map(|p| p.cost_per_input_token)
        .fold(f32::INFINITY, f32::min);
    let pre = candidates.len();
    candidates.retain(|p| (p.cost_per_input_token - min_cost).abs() < f32::EPSILON);
    if candidates.len() < pre {
        if let Some(winner) = candidates.first() {
            tie_breakers.push(TieBreaker::Cost {
                winner: winner.agent_id.clone(),
                cost_per_input_token: min_cost,
            });
        }
    }
    if candidates.len() == 1 {
        return finalize(
            candidates[0],
            candidate_ids,
            eliminated,
            tie_breakers,
            signature,
        );
    }

    // Step 5 — latency (fastest wins).
    let min_latency = candidates
        .iter()
        .map(|p| p.typical_latency_ms)
        .min()
        .unwrap_or(u32::MAX);
    let pre = candidates.len();
    candidates.retain(|p| p.typical_latency_ms == min_latency);
    if candidates.len() < pre {
        if let Some(winner) = candidates.first() {
            tie_breakers.push(TieBreaker::Latency {
                winner: winner.agent_id.clone(),
                typical_latency_ms: min_latency,
            });
        }
    }
    if candidates.len() == 1 {
        return finalize(
            candidates[0],
            candidate_ids,
            eliminated,
            tie_breakers,
            signature,
        );
    }

    // Step 6 — stable AgentId ordering (lexically lowest wins).
    candidates.sort_by(|a, b| a.agent_id.as_str().cmp(b.agent_id.as_str()));
    if let Some(winner) = candidates.first() {
        tie_breakers.push(TieBreaker::StableId {
            winner: winner.agent_id.clone(),
        });
    }
    finalize(
        candidates[0],
        candidate_ids,
        eliminated,
        tie_breakers,
        signature,
    )
}

fn finalize(
    chosen: &AgentCapabilityProfile,
    candidates: Vec<AgentId>,
    eliminated: Vec<(AgentId, EliminationReason)>,
    tie_breakers: Vec<TieBreaker>,
    signature: RequirementSignature,
) -> RoutingExplanation {
    RoutingExplanation {
        chosen: Some(chosen.agent_id.clone()),
        candidates,
        eliminated,
        tie_breakers_applied: tie_breakers,
        requirement_signature: signature,
    }
}

// ---------------------------------------------------------------------------
// StaticCapabilityRouter — application-layer test helper.
// ---------------------------------------------------------------------------

/// In-memory [`CapabilityRouterPort`] backed by a fixed profile list.
///
/// Equivalent role to
/// [`StaticReversibilityClassifier`](crate::reversibility_classifier::StaticReversibilityClassifier)
/// for the A2 substrate — hooks that take a `&dyn CapabilityRouterPort`
/// use this in their unit tests, with the production adapter (Phase 3b
/// in `sentinel-infrastructure`) doing TOML loading + filesystem
/// watching for hot config reload.
#[derive(Clone)]
pub struct StaticCapabilityRouter {
    profiles: Vec<AgentCapabilityProfile>,
    appraisal_store: Option<std::sync::Arc<dyn AppraisalStorePort>>,
    window: AppraisalWindow,
}

impl std::fmt::Debug for StaticCapabilityRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticCapabilityRouter")
            .field("profiles", &self.profiles.len())
            .field("has_appraisal_store", &self.appraisal_store.is_some())
            .field("window", &self.window)
            .finish()
    }
}

impl StaticCapabilityRouter {
    /// Empty router — useful as a base for `.with_agent` chaining.
    /// Routing against an empty router always returns
    /// [`RoutingError::NoAgentSatisfies`] with an empty unsatisfied
    /// list.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            profiles: Vec::new(),
            appraisal_store: None,
            window: DEFAULT_APPRAISAL_WINDOW,
        }
    }

    /// Construct from a static profile list.
    #[must_use]
    pub fn from_profiles(profiles: Vec<AgentCapabilityProfile>) -> Self {
        Self {
            profiles,
            appraisal_store: None,
            window: DEFAULT_APPRAISAL_WINDOW,
        }
    }

    /// Builder-style: append a profile.
    #[must_use]
    pub fn with_agent(mut self, profile: AgentCapabilityProfile) -> Self {
        self.profiles.push(profile);
        self
    }

    /// Builder-style: attach an appraisal store so tie-breaker step 3
    /// can fire.
    #[must_use]
    pub fn with_appraisal_store(mut self, store: std::sync::Arc<dyn AppraisalStorePort>) -> Self {
        self.appraisal_store = Some(store);
        self
    }

    /// Builder-style: override the appraisal window (defaults to
    /// [`DEFAULT_APPRAISAL_WINDOW`]).
    #[must_use]
    pub const fn with_window(mut self, window: AppraisalWindow) -> Self {
        self.window = window;
        self
    }
}

impl CapabilityRouterPort for StaticCapabilityRouter {
    fn route(&self, requirement: &CapabilityRequirement) -> Result<AgentId, RoutingError> {
        let explanation = pick(
            &self.profiles,
            requirement,
            self.appraisal_store.as_deref(),
            self.window,
        );
        explanation.chosen.ok_or_else(|| {
            // Collect unsatisfied diagnostics from every eliminated agent.
            let mut all_unsat: Vec<UnsatisfiedRequirement> = Vec::new();
            for (_id, reason) in &explanation.eliminated {
                if let EliminationReason::UnsatisfiedRequirement(items) = reason {
                    all_unsat.extend_from_slice(items);
                }
            }
            RoutingError::NoAgentSatisfies(all_unsat)
        })
    }

    fn candidates(&self, requirement: &CapabilityRequirement) -> Vec<AgentId> {
        // The pure pick() returns post-tie-break candidate set. For
        // `candidates()` we want the post-required/forbidden set
        // (before tie-breakers), so recompute that filter directly.
        let mut out = Vec::new();
        for profile in &self.profiles {
            if requirement.is_satisfied_by(profile) {
                out.push(profile.agent_id.clone());
            }
        }
        out
    }

    fn explain(&self, requirement: &CapabilityRequirement) -> RoutingExplanation {
        pick(
            &self.profiles,
            requirement,
            self.appraisal_store.as_deref(),
            self.window,
        )
    }
}

/// Convenience: construct from a tuple of `(profile, ...)` literals.
/// Mostly for test prose readability.
#[must_use]
pub fn static_router(profiles: Vec<AgentCapabilityProfile>) -> StaticCapabilityRouter {
    StaticCapabilityRouter::from_profiles(profiles)
}

// Suppress unused warning when the import is only used inside conditional code paths.
#[allow(dead_code)]
fn _capability_imports_used(_: Capability) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::capability::{
        AgentId, Capability, DataZone, ReasoningLevel, SchemaRef, VendorClass,
    };

    fn id(s: &str) -> AgentId {
        AgentId::new(s).unwrap()
    }

    fn opus() -> AgentCapabilityProfile {
        AgentCapabilityProfile {
            agent_id: id("opus"),
            display_name: "Opus 4.8".into(),
            vendor: VendorClass::Anthropic,
            model_id: "claude-opus-4.8".into(),
            declared: vec![
                Capability::Reasoning(ReasoningLevel::Deep),
                Capability::StructuredOutput(SchemaRef::AuditorVerdict),
                Capability::LatencyBudget(30000),
                Capability::CostBudget(0.50),
            ],
            cost_per_input_token: 0.000_015,
            cost_per_output_token: 0.000_075,
            typical_latency_ms: 6000,
            max_context_tokens: 200_000,
            data_zones: vec![DataZone::UsEast],
        }
    }

    fn kimi() -> AgentCapabilityProfile {
        AgentCapabilityProfile {
            agent_id: id("kimi"),
            display_name: "Kimi K2.6".into(),
            vendor: VendorClass::Ollama,
            model_id: "kimi-k2.6".into(),
            declared: vec![
                Capability::Reasoning(ReasoningLevel::Standard),
                Capability::StructuredOutput(SchemaRef::AuditorVerdict),
                Capability::LatencyBudget(15000),
                Capability::CostBudget(0.05),
            ],
            cost_per_input_token: 0.000_001,
            cost_per_output_token: 0.000_005,
            typical_latency_ms: 8000,
            max_context_tokens: 128_000,
            data_zones: vec![],
        }
    }

    fn gpt() -> AgentCapabilityProfile {
        AgentCapabilityProfile {
            agent_id: id("gpt"),
            display_name: "GPT 5.5".into(),
            vendor: VendorClass::Openai,
            model_id: "gpt-5.5".into(),
            declared: vec![
                Capability::Reasoning(ReasoningLevel::Deep),
                Capability::StructuredOutput(SchemaRef::AuditorVerdict),
                Capability::LatencyBudget(20000),
                Capability::CostBudget(0.30),
            ],
            cost_per_input_token: 0.000_005,
            cost_per_output_token: 0.000_020,
            typical_latency_ms: 4000,
            max_context_tokens: 256_000,
            data_zones: vec![DataZone::UsEast],
        }
    }

    // ---- Empty router ----

    #[test]
    fn empty_router_returns_no_agent_satisfies() {
        let r = StaticCapabilityRouter::empty();
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let err = r.route(&req).unwrap_err();
        assert!(matches!(err, RoutingError::NoAgentSatisfies(_)));
    }

    // ---- Required filtering ----

    #[test]
    fn route_picks_only_qualifying_agent() {
        let r = static_router(vec![opus(), kimi()]);
        // Require Deep reasoning — only opus qualifies.
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Deep)]);
        let chosen = r.route(&req).unwrap();
        assert_eq!(chosen, id("opus"));
    }

    #[test]
    fn route_eliminates_via_forbidden() {
        let r = static_router(vec![opus(), kimi()]);
        // Forbid Anthropic — opus eliminated; kimi remains.
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_forbidden(Capability::Vendor(VendorClass::Anthropic));
        let chosen = r.route(&req).unwrap();
        assert_eq!(chosen, id("kimi"));
    }

    #[test]
    fn route_realistic_a3_requirement_picks_different_vendor() {
        // A3 spec §5.1: acting=Anthropic → auditor must NOT be
        // Anthropic, must produce AuditorVerdict.
        let r = static_router(vec![opus(), kimi(), gpt()]);
        let req = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Standard),
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
            Capability::StructuredOutput(SchemaRef::AuditorVerdict),
        ])
        .with_preferred(Capability::Reasoning(ReasoningLevel::Deep));
        // Kimi: Standard reasoning, non-Anthropic, has AuditorVerdict.
        // GPT: Deep reasoning, non-Anthropic, has AuditorVerdict.
        // Preferred Deep → GPT wins on tie-breaker step 2.
        let chosen = r.route(&req).unwrap();
        assert_eq!(chosen, id("gpt"));
    }

    // ---- Tie-breaker order ----

    #[test]
    fn tie_breaker_step2_preferred_wins() {
        let r = static_router(vec![opus(), kimi()]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_preferred(Capability::Reasoning(ReasoningLevel::Deep));
        // Both qualify on required; opus has Deep, kimi has Standard
        // → preferred score: opus=1, kimi=0.
        let chosen = r.route(&req).unwrap();
        assert_eq!(chosen, id("opus"));
    }

    #[test]
    fn tie_breaker_step4_cost_wins_when_preferred_ties() {
        let r = static_router(vec![opus(), gpt()]);
        // Both Deep; no preferred capability → tie on step 2.
        // gpt cost_per_input_token = 0.000_005, opus = 0.000_015.
        // gpt is cheaper → wins on step 4.
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Deep)]);
        let chosen = r.route(&req).unwrap();
        assert_eq!(chosen, id("gpt"));
    }

    #[test]
    fn tie_breaker_step5_latency_wins_when_cost_ties() {
        // Build two profiles with identical cost so step 4 ties.
        let mut twin_a = kimi();
        twin_a.agent_id = id("twin-a");
        twin_a.typical_latency_ms = 10000;
        let mut twin_b = kimi();
        twin_b.agent_id = id("twin-b");
        twin_b.typical_latency_ms = 5000;
        let r = static_router(vec![twin_a, twin_b]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let chosen = r.route(&req).unwrap();
        assert_eq!(chosen, id("twin-b"), "lower latency wins after cost tie");
    }

    #[test]
    fn tie_breaker_step6_stable_id_final_fallback() {
        // Two identical profiles (cost + latency + reasoning) — only
        // step 6 can decide.
        let mut a = kimi();
        a.agent_id = id("zeta");
        let mut b = kimi();
        b.agent_id = id("alpha");
        let r = static_router(vec![a, b]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)]);
        let chosen = r.route(&req).unwrap();
        assert_eq!(
            chosen,
            id("alpha"),
            "lexically lowest wins as final fallback"
        );
    }

    // ---- candidates() ----

    #[test]
    fn candidates_returns_pre_tie_break_set() {
        let r = static_router(vec![opus(), kimi(), gpt()]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_preferred(Capability::Reasoning(ReasoningLevel::Deep));
        let mut cands = r.candidates(&req);
        cands.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(cands, vec![id("gpt"), id("kimi"), id("opus")]);
    }

    // ---- explain() ----

    #[test]
    fn explain_records_eliminated_with_reasons() {
        let r = static_router(vec![opus(), kimi()]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Deep)]);
        let expl = r.explain(&req);
        assert_eq!(expl.chosen, Some(id("opus")));
        assert_eq!(expl.candidates, vec![id("opus")]);
        assert_eq!(expl.eliminated.len(), 1);
        let (eliminated_id, reason) = &expl.eliminated[0];
        assert_eq!(eliminated_id, &id("kimi"));
        assert!(matches!(
            reason,
            EliminationReason::UnsatisfiedRequirement(_)
        ));
    }

    #[test]
    fn explain_records_forbidden_match() {
        let r = static_router(vec![opus(), kimi()]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_forbidden(Capability::Vendor(VendorClass::Anthropic));
        let expl = r.explain(&req);
        let (eliminated_id, reason) = &expl.eliminated[0];
        assert_eq!(eliminated_id, &id("opus"));
        assert!(matches!(
            reason,
            EliminationReason::ForbiddenCapabilityMatched(_)
        ));
    }

    #[test]
    fn explain_records_fired_tie_breakers() {
        let r = static_router(vec![opus(), kimi()]);
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_preferred(Capability::Reasoning(ReasoningLevel::Deep));
        let expl = r.explain(&req);
        assert_eq!(expl.chosen, Some(id("opus")));
        assert!(
            expl.tie_breakers_applied
                .iter()
                .any(|t| matches!(t, TieBreaker::PreferredScore { .. })),
            "preferred-score tie-breaker should have fired"
        );
    }

    // ---- Send + Sync ----

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StaticCapabilityRouter>();
    }
}
