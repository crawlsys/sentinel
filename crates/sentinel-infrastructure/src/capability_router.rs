//! A2 Phase 3b — TOML-backed `CapabilityRouterPort` adapter.
//!
//! Loads [`AgentCapabilityProfile`]s from one or more TOML files and
//! routes capability requirements through the
//! [`sentinel_application::capability_router::pick`] algorithm — the
//! Phase 3a shared implementation. The TOML adapter is thin: it owns
//! config loading + profile validation; the routing algorithm itself
//! is the same code the [`StaticCapabilityRouter`] test helper uses,
//! so behavior is identical across adapters.
//!
//! ## Loading
//!
//! - **Shipped defaults** — `config/agents-defaults.toml` baked in via
//!   `include_str!` at compile time. Contains a small set of common
//!   frontier model profiles (Claude Opus 4.8, Kimi K2.6 on Ollama
//!   Cloud, GPT 5.5) so a fresh install has a router that can route.
//! - **Operator overrides** — `~/.claude/sentinel/config/agents.toml`
//!   (optional, runtime-loaded). Operators register their own
//!   profiles or override the shipped ones by `agent_id`. The merge
//!   rule: same-id profiles in the overrides file replace shipped
//!   ones; new-id profiles append.
//!
//! ## Validation
//!
//! Loaded at startup. Failures (malformed TOML, unknown vendor,
//! negative cost, etc.) surface as [`anyhow::Error`] from the loader
//! — sentinel refuses to start with invalid profiles rather than
//! route to ghost agents.
//!
//! [`StaticCapabilityRouter`]: sentinel_application::capability_router::StaticCapabilityRouter

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use sentinel_application::capability_router::pick;
use sentinel_domain::agent_routing::{AppraisalWindow, RoutingExplanation};
use sentinel_domain::capability::{AgentCapabilityProfile, AgentId, CapabilityRequirement};
use sentinel_domain::ports::{
    AppraisalStorePort, CapabilityRouterPort, RoutingError,
};

/// Shipped default profiles (Phase 3b baseline). Operators replace or
/// extend in `~/.claude/sentinel/config/agents.toml`.
pub const SHIPPED_AGENTS_DEFAULTS: &str = include_str!("../../../config/agents-defaults.toml");

// ---------------------------------------------------------------------------
// TOML schema
// ---------------------------------------------------------------------------

/// Top-level TOML structure. Both the shipped defaults file and the
/// operator overrides file share this schema; the loader merges them.
#[derive(Debug, Default, Deserialize)]
pub struct AgentsConfigToml {
    /// `[[agent]]` — one entry per registered agent.
    #[serde(default)]
    pub agent: Vec<AgentCapabilityProfile>,
}

// ---------------------------------------------------------------------------
// TomlCapabilityRouter
// ---------------------------------------------------------------------------

/// Production [`CapabilityRouterPort`] adapter.
///
/// Built once at session start from the shipped defaults + optional
/// operator overrides; held by the hook engine for the lifetime of
/// the session. Profile list is immutable after construction — hot
/// reload is operator-driven (restart sentinel after editing
/// `agents.toml`).
pub struct TomlCapabilityRouter {
    profiles: Vec<AgentCapabilityProfile>,
    appraisal_store: Option<Arc<dyn AppraisalStorePort>>,
    window: AppraisalWindow,
}

impl std::fmt::Debug for TomlCapabilityRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TomlCapabilityRouter")
            .field("profiles", &self.profiles.len())
            .field("has_appraisal_store", &self.appraisal_store.is_some())
            .field("window", &self.window)
            .finish()
    }
}

impl TomlCapabilityRouter {
    /// Construct from already-parsed profiles. Bypasses TOML loading
    /// — used by tests that supply hand-crafted profile lists.
    #[must_use]
    pub fn from_profiles(profiles: Vec<AgentCapabilityProfile>) -> Self {
        Self {
            profiles,
            appraisal_store: None,
            window: sentinel_application::capability_router::DEFAULT_APPRAISAL_WINDOW,
        }
    }

    /// Load profiles from the shipped defaults TOML only. Used when
    /// no operator overrides file exists yet.
    pub fn with_shipped_defaults() -> Result<Self> {
        let config: AgentsConfigToml = toml::from_str(SHIPPED_AGENTS_DEFAULTS)
            .context("failed to parse shipped agents-defaults.toml")?;
        validate_profiles(&config.agent)?;
        Ok(Self::from_profiles(config.agent))
    }

    /// Load profiles from a TOML string. Validates schema + profile
    /// values; returns an error naming the bad profile on failure.
    pub fn from_toml_str(s: &str) -> Result<Self> {
        let config: AgentsConfigToml =
            toml::from_str(s).context("failed to parse agents TOML")?;
        validate_profiles(&config.agent)?;
        Ok(Self::from_profiles(config.agent))
    }

    /// Load profiles from a TOML file at `path`. Convenience wrapper
    /// around [`Self::from_toml_str`] with `std::fs::read_to_string`.
    pub fn from_toml_path(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).with_context(|| {
            format!("failed to read agents TOML from {}", path.display())
        })?;
        Self::from_toml_str(&content)
    }

    /// Load shipped defaults, then merge an optional operator
    /// overrides file. Same-id profiles in the overrides REPLACE
    /// shipped ones; new-id profiles APPEND. Returns the shipped-only
    /// router when `overrides_path` is `None` or the file doesn't
    /// exist (operator hasn't customized yet).
    pub fn with_shipped_and_overrides(overrides_path: Option<&Path>) -> Result<Self> {
        let mut shipped = Self::with_shipped_defaults()?.profiles;
        if let Some(path) = overrides_path {
            if !path.exists() {
                tracing::debug!(
                    "no operator overrides file at {} — using shipped defaults only",
                    path.display()
                );
                return Ok(Self::from_profiles(shipped));
            }
            let overrides_content = std::fs::read_to_string(path).with_context(|| {
                format!("failed to read agents overrides TOML from {}", path.display())
            })?;
            let overrides: AgentsConfigToml = toml::from_str(&overrides_content)
                .with_context(|| {
                    format!("failed to parse operator overrides TOML at {}", path.display())
                })?;
            validate_profiles(&overrides.agent)?;
            merge_overrides(&mut shipped, overrides.agent);
        }
        Ok(Self::from_profiles(shipped))
    }

    /// Attach an [`AppraisalStorePort`] so tie-breaker step 3
    /// (success-rate) can fire. Without a store, the chain falls
    /// through directly to cost/latency.
    #[must_use]
    pub fn with_appraisal_store(mut self, store: Arc<dyn AppraisalStorePort>) -> Self {
        self.appraisal_store = Some(store);
        self
    }

    /// Override the appraisal window (defaults to
    /// `DEFAULT_APPRAISAL_WINDOW` = `LastN(200)`).
    #[must_use]
    pub const fn with_window(mut self, window: AppraisalWindow) -> Self {
        self.window = window;
        self
    }

    /// Read-only access to the registered profiles. Useful for
    /// operator-facing tooling that wants to render the catalog.
    #[must_use]
    pub fn profiles(&self) -> &[AgentCapabilityProfile] {
        &self.profiles
    }
}

impl CapabilityRouterPort for TomlCapabilityRouter {
    fn route(&self, requirement: &CapabilityRequirement) -> Result<AgentId, RoutingError> {
        let explanation = pick(
            &self.profiles,
            requirement,
            self.appraisal_store.as_deref(),
            self.window,
        );
        explanation.chosen.ok_or_else(|| {
            let mut all_unsat = Vec::new();
            for (_id, reason) in &explanation.eliminated {
                if let sentinel_domain::agent_routing::EliminationReason::UnsatisfiedRequirement(
                    items,
                ) = reason
                {
                    all_unsat.extend_from_slice(items);
                }
            }
            RoutingError::NoAgentSatisfies(all_unsat)
        })
    }

    fn candidates(&self, requirement: &CapabilityRequirement) -> Vec<AgentId> {
        self.profiles
            .iter()
            .filter(|p| requirement.is_satisfied_by(p))
            .map(|p| p.agent_id.clone())
            .collect()
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

// ---------------------------------------------------------------------------
// Profile validation + merge
// ---------------------------------------------------------------------------

fn validate_profiles(profiles: &[AgentCapabilityProfile]) -> Result<()> {
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for p in profiles {
        if !seen_ids.insert(p.agent_id.as_str()) {
            return Err(anyhow!(
                "duplicate agent_id {:?} in profile list — every agent_id must be unique within a file",
                p.agent_id.as_str()
            ));
        }
        if p.cost_per_input_token < 0.0 || p.cost_per_output_token < 0.0 {
            return Err(anyhow!(
                "agent {:?} has negative cost — costs must be ≥ 0",
                p.agent_id.as_str()
            ));
        }
        if p.typical_latency_ms == 0 {
            return Err(anyhow!(
                "agent {:?} has typical_latency_ms = 0 — must be > 0",
                p.agent_id.as_str()
            ));
        }
        if p.typical_latency_ms > 300_000 {
            return Err(anyhow!(
                "agent {:?} has typical_latency_ms = {} — must be ≤ 300_000 (5 minutes)",
                p.agent_id.as_str(),
                p.typical_latency_ms
            ));
        }
        if p.max_context_tokens == 0 {
            return Err(anyhow!(
                "agent {:?} has max_context_tokens = 0 — must be > 0",
                p.agent_id.as_str()
            ));
        }
        if p.display_name.trim().is_empty() {
            return Err(anyhow!(
                "agent {:?} has empty display_name — required for routing-explain output",
                p.agent_id.as_str()
            ));
        }
    }
    Ok(())
}

/// Merge overrides into the shipped baseline. Same-`agent_id` profiles
/// in `overrides` REPLACE the shipped versions; new-id profiles APPEND.
fn merge_overrides(
    shipped: &mut Vec<AgentCapabilityProfile>,
    overrides: Vec<AgentCapabilityProfile>,
) {
    for ovr in overrides {
        if let Some(pos) = shipped
            .iter()
            .position(|p| p.agent_id == ovr.agent_id)
        {
            shipped[pos] = ovr;
        } else {
            shipped.push(ovr);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::capability::{Capability, ReasoningLevel, SchemaRef, VendorClass};

    const MINIMAL_PROFILE_TOML: &str = r#"
[[agent]]
agent_id = "kimi"
display_name = "Kimi K2.6"
vendor = "Ollama"
model_id = "kimi-k2.6"
declared = [
    { Reasoning = "standard" },
    { StructuredOutput = "AuditorVerdict" },
]
cost_per_input_token = 0.0000010
cost_per_output_token = 0.0000050
typical_latency_ms = 8000
max_context_tokens = 128000
data_zones = []
"#;

    const TWO_AGENT_TOML: &str = r#"
[[agent]]
agent_id = "kimi"
display_name = "Kimi K2.6"
vendor = "Ollama"
model_id = "kimi-k2.6"
declared = [
    { Reasoning = "standard" },
    { StructuredOutput = "AuditorVerdict" },
]
cost_per_input_token = 0.0000010
cost_per_output_token = 0.0000050
typical_latency_ms = 8000
max_context_tokens = 128000
data_zones = []

[[agent]]
agent_id = "opus"
display_name = "Claude Opus 4.8"
vendor = "Anthropic"
model_id = "claude-opus-4.8"
declared = [
    { Reasoning = "deep" },
    { StructuredOutput = "AuditorVerdict" },
]
cost_per_input_token = 0.000015
cost_per_output_token = 0.000075
typical_latency_ms = 6000
max_context_tokens = 200000
data_zones = ["UsEast"]
"#;

    // ---- Shipped defaults load + parse ----

    #[test]
    fn shipped_defaults_loads_cleanly() {
        let router = TomlCapabilityRouter::with_shipped_defaults().unwrap();
        assert!(
            !router.profiles().is_empty(),
            "shipped defaults must register at least one agent"
        );
    }

    // ---- TOML parsing ----

    #[test]
    fn parses_minimal_profile() {
        let router = TomlCapabilityRouter::from_toml_str(MINIMAL_PROFILE_TOML).unwrap();
        assert_eq!(router.profiles().len(), 1);
        let p = &router.profiles()[0];
        assert_eq!(p.agent_id.as_str(), "kimi");
        assert_eq!(p.vendor, VendorClass::Ollama);
        assert_eq!(p.declared.len(), 2);
    }

    #[test]
    fn parses_multiple_agents() {
        let router = TomlCapabilityRouter::from_toml_str(TWO_AGENT_TOML).unwrap();
        assert_eq!(router.profiles().len(), 2);
    }

    #[test]
    fn malformed_toml_returns_error() {
        let err = TomlCapabilityRouter::from_toml_str("not valid toml [[[").unwrap_err();
        assert!(format!("{err:#}").contains("parse"));
    }

    #[test]
    fn unknown_vendor_returns_error() {
        // Note: VendorClass is an enum with explicit variants; unknown
        // variant string surfaces as a deserialize error at TOML parse
        // time.
        let bad = r#"
[[agent]]
agent_id = "x"
display_name = "X"
vendor = "TotallyMadeUp"
model_id = "fake"
declared = []
cost_per_input_token = 0.0
cost_per_output_token = 0.0
typical_latency_ms = 1000
max_context_tokens = 1000
data_zones = []
"#;
        let err = TomlCapabilityRouter::from_toml_str(bad).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("TotallyMadeUp") || msg.contains("variant"),
            "error should name the bad vendor: {msg}"
        );
    }

    // ---- Validation rules ----

    #[test]
    fn rejects_duplicate_agent_id() {
        let dup = r#"
[[agent]]
agent_id = "twin"
display_name = "First"
vendor = "Anthropic"
model_id = "x"
declared = []
cost_per_input_token = 0.0
cost_per_output_token = 0.0
typical_latency_ms = 1000
max_context_tokens = 1000
data_zones = []

[[agent]]
agent_id = "twin"
display_name = "Second"
vendor = "Anthropic"
model_id = "y"
declared = []
cost_per_input_token = 0.0
cost_per_output_token = 0.0
typical_latency_ms = 1000
max_context_tokens = 1000
data_zones = []
"#;
        let err = TomlCapabilityRouter::from_toml_str(dup).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate agent_id"));
    }

    #[test]
    fn rejects_negative_cost() {
        let bad = r#"
[[agent]]
agent_id = "negcost"
display_name = "Negative"
vendor = "Anthropic"
model_id = "x"
declared = []
cost_per_input_token = -0.01
cost_per_output_token = 0.0
typical_latency_ms = 1000
max_context_tokens = 1000
data_zones = []
"#;
        let err = TomlCapabilityRouter::from_toml_str(bad).unwrap_err();
        assert!(format!("{err:#}").contains("negative cost"));
    }

    #[test]
    fn rejects_zero_latency() {
        let bad = r#"
[[agent]]
agent_id = "zerolat"
display_name = "ZeroLatency"
vendor = "Anthropic"
model_id = "x"
declared = []
cost_per_input_token = 0.0
cost_per_output_token = 0.0
typical_latency_ms = 0
max_context_tokens = 1000
data_zones = []
"#;
        let err = TomlCapabilityRouter::from_toml_str(bad).unwrap_err();
        assert!(format!("{err:#}").contains("typical_latency_ms = 0"));
    }

    #[test]
    fn rejects_latency_over_300k() {
        let bad = r#"
[[agent]]
agent_id = "slowpoke"
display_name = "SlowPoke"
vendor = "Anthropic"
model_id = "x"
declared = []
cost_per_input_token = 0.0
cost_per_output_token = 0.0
typical_latency_ms = 999999
max_context_tokens = 1000
data_zones = []
"#;
        let err = TomlCapabilityRouter::from_toml_str(bad).unwrap_err();
        assert!(format!("{err:#}").contains("300_000"));
    }

    #[test]
    fn rejects_empty_display_name() {
        let bad = r#"
[[agent]]
agent_id = "noname"
display_name = "   "
vendor = "Anthropic"
model_id = "x"
declared = []
cost_per_input_token = 0.0
cost_per_output_token = 0.0
typical_latency_ms = 1000
max_context_tokens = 1000
data_zones = []
"#;
        let err = TomlCapabilityRouter::from_toml_str(bad).unwrap_err();
        assert!(format!("{err:#}").contains("empty display_name"));
    }

    // ---- merge_overrides ----

    #[test]
    fn merge_overrides_replaces_same_id() {
        let mut shipped =
            TomlCapabilityRouter::from_toml_str(TWO_AGENT_TOML).unwrap().profiles;
        let mut override_profile = shipped[0].clone();
        override_profile.cost_per_input_token = 9999.0;
        merge_overrides(&mut shipped, vec![override_profile]);
        let kimi = shipped.iter().find(|p| p.agent_id.as_str() == "kimi").unwrap();
        assert!(
            (kimi.cost_per_input_token - 9999.0).abs() < 1e-3,
            "operator override should replace shipped cost"
        );
    }

    #[test]
    fn merge_overrides_appends_new_id() {
        let mut shipped =
            TomlCapabilityRouter::from_toml_str(TWO_AGENT_TOML).unwrap().profiles;
        let before = shipped.len();
        let new = TomlCapabilityRouter::from_toml_str(MINIMAL_PROFILE_TOML)
            .unwrap()
            .profiles[0]
            .clone();
        // Use a fresh id to ensure append:
        let mut fresh = new;
        fresh.agent_id = AgentId::new("brand-new").unwrap();
        merge_overrides(&mut shipped, vec![fresh]);
        assert_eq!(shipped.len(), before + 1);
        assert!(shipped.iter().any(|p| p.agent_id.as_str() == "brand-new"));
    }

    // ---- End-to-end route through the TOML router ----

    #[test]
    fn route_uses_pure_pick_algorithm() {
        let router = TomlCapabilityRouter::from_toml_str(TWO_AGENT_TOML).unwrap();
        // Same fixture as the static-router tests use; same expected result.
        let req = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Standard),
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
            Capability::StructuredOutput(SchemaRef::AuditorVerdict),
        ]);
        let chosen = router.route(&req).unwrap();
        assert_eq!(chosen.as_str(), "kimi");
    }

    #[test]
    fn no_agent_satisfies_carries_diagnostics() {
        let router = TomlCapabilityRouter::from_toml_str(MINIMAL_PROFILE_TOML).unwrap();
        // Kimi declares Standard; require Deep — no candidate.
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Deep)]);
        let err = router.route(&req).unwrap_err();
        match err {
            RoutingError::NoAgentSatisfies(unsat) => {
                assert!(!unsat.is_empty(), "diagnostics should carry the nearest miss");
            }
            other @ RoutingError::Configuration(_) => {
                panic!("expected NoAgentSatisfies, got {other:?}")
            }
        }
    }

    #[test]
    fn router_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TomlCapabilityRouter>();
    }
}
