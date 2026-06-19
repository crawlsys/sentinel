//! A2 — Capability domain types.
//!
//! Per `docs/a2-capability-aware-routing.md` §2. Pure value objects that
//! describe (a) **what work needs** (a [`CapabilityRequirement`]) and
//! (b) **what an agent can satisfy** (an [`AgentCapabilityProfile`]).
//! The substrate that consuming hooks (A3 auditor selection, BA5 critic
//! selection, future budget-coder routing) query to ask "which agent
//! handles this?" without hardcoding vendor pairings.
//!
//! No IO, no business logic — that lives in the application/infra
//! layers behind [`crate::ports::CapabilityRouterPort`].

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// AgentId — stable identifier for a registered agent.
// ---------------------------------------------------------------------------

/// Stable, operator-managed identifier for a registered agent (a
/// `model_id` + `system_prompt` + `tool_access` combination).
///
/// `AgentId` strings live in `config/agents.toml`. The
/// [`crate::ports::CapabilityRouterPort::route`] return value is an
/// `AgentId`; consumers resolve the id to a concrete client through the
/// infrastructure layer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AgentId(String);

impl AgentId {
    /// Construct an `AgentId`. Trims whitespace; empty after-trim is
    /// rejected because routing on `""` is always a bug.
    pub fn new(s: impl Into<String>) -> Result<Self, AgentIdError> {
        let trimmed = s.into().trim().to_string();
        if trimmed.is_empty() {
            return Err(AgentIdError::Empty);
        }
        Ok(Self(trimmed))
    }

    /// Borrow as `&str` for display + serialization.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Construction-time errors for [`AgentId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentIdError {
    /// Empty (or whitespace-only) string passed to [`AgentId::new`].
    Empty,
}

impl fmt::Display for AgentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("AgentId must not be empty"),
        }
    }
}

impl std::error::Error for AgentIdError {}

// ---------------------------------------------------------------------------
// VendorClass — coarse vendor identity for A3's same-vendor-separation rule.
// ---------------------------------------------------------------------------

/// Coarse vendor identity for an agent.
///
/// Powers A3's `DifferentVendorFrom(acting_vendor)` constraint — the
/// auditor must come from a different vendor than the acting agent.
/// `Other` carries the underlying vendor when the agent is accessed via
/// a gateway (`OpenRouter`, mcp-router) so `DifferentVendorFrom`
/// comparisons see the *true* upstream vendor, not the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VendorClass {
    Anthropic,
    Openai,
    Google,
    Xai,
    Meta,
    Mistral,
    Ollama,
    Openrouter,
    /// Used when the agent is accessed via a gateway (e.g. `OpenRouter`
    /// fronting Claude). `underlying_vendor` is the model's actual
    /// vendor; [`Self::true_vendor`] returns it.
    Other {
        underlying_vendor: Box<Self>,
    },
    /// Truly-third-party / unidentified — comparisons treat as a
    /// distinct vendor for [`Self::is_different_from`] purposes.
    Unknown,
}

impl VendorClass {
    /// Resolve to the underlying vendor when wrapped in [`Self::Other`].
    /// Returns `&self` for non-`Other` variants.
    #[must_use]
    pub fn true_vendor(&self) -> &Self {
        match self {
            Self::Other { underlying_vendor } => underlying_vendor.true_vendor(),
            other => other,
        }
    }

    /// Returns `true` iff this vendor's *true upstream* differs from
    /// `other`'s. `Unknown` is treated as distinct from every named
    /// vendor including another `Unknown` (separate calls are not
    /// presumed to be the same provider).
    #[must_use]
    pub fn is_different_from(&self, other: &Self) -> bool {
        let a = self.true_vendor();
        let b = other.true_vendor();
        if matches!(a, Self::Unknown) || matches!(b, Self::Unknown) {
            return true;
        }
        a != b
    }
}

// ---------------------------------------------------------------------------
// ReasoningLevel — ordered reasoning-depth capability.
// ---------------------------------------------------------------------------

/// Ordered reasoning-depth tier. `Shallow < Standard < Deep`. Used both
/// as a required capability (`Capability::Reasoning(level)`) and as the
/// declared capability of an agent profile.
///
/// `PartialOrd` is implemented so `agent.has_at_least(required)` is
/// expressible directly: an agent declaring `Deep` satisfies a
/// requirement of `Standard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    Shallow,
    Standard,
    Deep,
}

impl ReasoningLevel {
    const fn rank(self) -> u8 {
        match self {
            Self::Shallow => 0,
            Self::Standard => 1,
            Self::Deep => 2,
        }
    }

    /// Returns `true` iff `self`'s tier is at least as deep as `other`.
    #[must_use]
    pub const fn at_least(self, other: Self) -> bool {
        self.rank() >= other.rank()
    }
}

impl PartialOrd for ReasoningLevel {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ReasoningLevel {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank().cmp(&other.rank())
    }
}

// ---------------------------------------------------------------------------
// ToolKind — coarse tool-class capability for ToolUse requirements.
// ---------------------------------------------------------------------------

/// Coarse class of tool the agent can call. Required-tool sets are
/// expressed as `Capability::ToolUse(vec![...])`; an agent's declared
/// `ToolUse` set must be a superset of the required set.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ToolKind {
    Read,
    Glob,
    Grep,
    Edit,
    Write,
    Bash,
    Task,
    TaskUpdate,
    WebFetch,
    WebSearch,
    /// Tool with a name not enumerated here (e.g. MCP tools from
    /// arbitrary servers). Carried as a string so the requirement can
    /// pin specific MCP tools (`Other("mcp__linear__list_issues")`)
    /// without expanding the enum.
    Other(String),
}

// ---------------------------------------------------------------------------
// SchemaRef — schema reference for StructuredOutput capability.
// ---------------------------------------------------------------------------

/// Reference to a structured-output schema the agent must be able to
/// produce.
///
/// Powers `Capability::StructuredOutput(SchemaRef::Auditor)`,
/// `SchemaRef::BaCritique`, etc. — agents that don't reliably follow
/// strict-JSON instructions are filtered out at requirement time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SchemaRef {
    /// `AuditorVerdict` per `dry_run.rs` — for A3 auditor seats.
    AuditorVerdict,
    /// BA critique schema — for BA5 critic seats.
    BaCritique,
    /// Any structured JSON (looser — agent reliably produces JSON of
    /// any shape requested in the prompt).
    AnyJson,
    /// Named schema not enumerated here.
    Named(String),
}

// ---------------------------------------------------------------------------
// DataZone — reserved for data-locality routing.
// ---------------------------------------------------------------------------

/// Geographic / regulatory data zone an agent's inference runs in.
///
/// **Reserved for A2 v2** — the field is wired now so requirements can
/// declare zones, but enforcement (which zone the API actually serves
/// from) needs operator-supplied metadata that v1 doesn't ship.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataZone {
    UsEast,
    UsWest,
    Eu,
    OnPremise,
    /// Named zone for custom operator zones.
    Named(String),
}

// ---------------------------------------------------------------------------
// Capability — the atomic constraint.
// ---------------------------------------------------------------------------

/// An atomic capability constraint. Used in two directions:
///
/// - As a *required* capability on a [`CapabilityRequirement`] — the
///   router must find an agent that satisfies this.
/// - As a *provided* capability on an [`AgentCapabilityProfile`] — the
///   agent declares it can satisfy this.
///
/// Cost / latency variants compare with `<=` semantics: an agent's
/// `CostBudget(0.05)` declaration satisfies a requirement's
/// `CostBudget(0.10)` (agent is within budget).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Capability {
    /// Required reasoning depth. Agent must provide a level at least
    /// as deep.
    Reasoning(ReasoningLevel),

    /// Required set of tools. Agent must declare a superset.
    ToolUse(Vec<ToolKind>),

    /// Required structured-output schema. Agent must declare it.
    StructuredOutput(SchemaRef),

    /// Vendor must be this exact vendor.
    Vendor(VendorClass),

    /// Vendor must NOT be this vendor (true-vendor comparison).
    /// Powers A3's separate-vendor-auditor rule.
    DifferentVendorFrom(VendorClass),

    /// Agent must be open-weights / locally runnable. Used by A8
    /// interpretability probes and the strict-privacy path.
    OpenWeights,

    /// Agent's typical latency must be within budget (milliseconds).
    LatencyBudget(u32),

    /// Agent's cost-per-call must be within budget (USD).
    CostBudget(f32),

    /// Agent must be qualified for this reversibility class. Powers
    /// A6 intersection: Catastrophic-class work demands the strongest
    /// reasoning available.
    ReversibilityClass(crate::reversibility::ReversibilityClass),

    /// Agent's inference must run in this data zone. Reserved for v2
    /// enforcement.
    DataLocality(DataZone),
}

impl Capability {
    /// Returns `true` iff `profile` provides this capability.
    /// Resolution rules:
    ///
    /// - `Reasoning(req)` — match if profile declares `Reasoning(prof)`
    ///   with `prof.at_least(req)`.
    /// - `ToolUse(req)` — match if profile declares `ToolUse(prof)`
    ///   with `prof` a superset of `req`.
    /// - `Vendor(req)` — match if profile's vendor true-equals `req`.
    /// - `DifferentVendorFrom(forbidden)` — match if profile's
    ///   true-vendor differs from `forbidden`.
    /// - `CostBudget(budget)` — match if profile's declared
    ///   `CostBudget(actual)` has `actual <= budget`.
    /// - `LatencyBudget(budget)` — same `<=` semantics.
    /// - `OpenWeights` — match if profile declares it.
    /// - `StructuredOutput(req)` — match if profile declares the same.
    ///   `AnyJson` is satisfied by any specific schema declaration.
    /// - `ReversibilityClass(req)` — match if profile declares the same.
    /// - `DataLocality(req)` — match if profile's `data_zones` contains
    ///   the required zone (v1: best-effort; v2 enforces).
    #[must_use]
    pub fn is_satisfied_by(&self, profile: &AgentCapabilityProfile) -> bool {
        match self {
            Self::Reasoning(req_level) => profile.declared.iter().any(|c| match c {
                Self::Reasoning(prof_level) => prof_level.at_least(*req_level),
                _ => false,
            }),
            Self::ToolUse(req_tools) => profile.declared.iter().any(|c| match c {
                Self::ToolUse(prof_tools) => req_tools.iter().all(|t| prof_tools.contains(t)),
                _ => false,
            }),
            Self::StructuredOutput(req_schema) => profile.declared.iter().any(|c| match c {
                Self::StructuredOutput(prof_schema) => {
                    prof_schema == req_schema
                        || matches!(req_schema, SchemaRef::AnyJson)
                            && !matches!(prof_schema, SchemaRef::AnyJson)
                }
                _ => false,
            }),
            Self::Vendor(req_vendor) => profile.vendor.true_vendor() == req_vendor.true_vendor(),
            Self::DifferentVendorFrom(forbidden) => profile.vendor.is_different_from(forbidden),
            Self::OpenWeights => profile
                .declared
                .iter()
                .any(|c| matches!(c, Self::OpenWeights)),
            Self::LatencyBudget(budget) => profile.declared.iter().any(|c| match c {
                Self::LatencyBudget(actual) => actual <= budget,
                _ => false,
            }),
            Self::CostBudget(budget) => profile.declared.iter().any(|c| match c {
                Self::CostBudget(actual) => actual <= budget,
                _ => false,
            }),
            Self::ReversibilityClass(req_class) => profile.declared.iter().any(|c| match c {
                Self::ReversibilityClass(prof_class) => prof_class.at_least(*req_class),
                _ => false,
            }),
            Self::DataLocality(req_zone) => profile.data_zones.contains(req_zone),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentCapabilityProfile — what an agent declares it can do.
// ---------------------------------------------------------------------------

/// Operator-managed declaration of an agent's capabilities and cost
/// characteristics. Lives in `config/agents.toml`; the routing adapter
/// loads them at startup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCapabilityProfile {
    /// Stable identifier the router returns.
    pub agent_id: AgentId,

    /// Operator-facing display name (for `routing explain` output).
    pub display_name: String,

    /// Coarse vendor class (with optional `underlying_vendor` for
    /// gateway-fronted agents).
    pub vendor: VendorClass,

    /// Concrete model identifier passed to the inference client.
    pub model_id: String,

    /// Capabilities this profile satisfies.
    pub declared: Vec<Capability>,

    /// Approximate cost per input token (USD).
    pub cost_per_input_token: f32,

    /// Approximate cost per output token (USD).
    pub cost_per_output_token: f32,

    /// Operator-reported typical latency (ms). Used as a tie-breaker
    /// and to satisfy `LatencyBudget` requirements indirectly.
    pub typical_latency_ms: u32,

    /// Maximum context window in tokens.
    pub max_context_tokens: u32,

    /// Where the inference runs (for `DataLocality` requirements).
    /// Empty in v1 — v2 enforces.
    pub data_zones: Vec<DataZone>,
}

// ---------------------------------------------------------------------------
// CapabilityRequirement — what a work item asks the router for.
// ---------------------------------------------------------------------------

/// Capability constraint set a routing call must satisfy.
///
/// - `required` — every entry must be satisfied by the chosen agent.
///   Unsatisfied → agent eliminated.
/// - `preferred` — tie-breaker; one point per preferred-satisfied.
///   Unsatisfied preferred capabilities do not eliminate.
/// - `forbidden` — any satisfied entry eliminates the agent. Used to
///   ban specific vendors / open-weights / catastrophic-cost shortcuts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub required: Vec<Capability>,
    #[serde(default)]
    pub preferred: Vec<Capability>,
    #[serde(default)]
    pub forbidden: Vec<Capability>,
}

impl CapabilityRequirement {
    /// Construct a requirement with only `required` entries.
    /// `preferred` and `forbidden` default to empty.
    #[must_use]
    pub const fn new(required: Vec<Capability>) -> Self {
        Self {
            required,
            preferred: Vec::new(),
            forbidden: Vec::new(),
        }
    }

    /// Builder-style: add a preferred (tie-breaker) capability.
    #[must_use]
    pub fn with_preferred(mut self, c: Capability) -> Self {
        self.preferred.push(c);
        self
    }

    /// Builder-style: add a forbidden capability.
    #[must_use]
    pub fn with_forbidden(mut self, c: Capability) -> Self {
        self.forbidden.push(c);
        self
    }

    /// Returns `true` iff every `required` capability is satisfied by
    /// `profile` AND no `forbidden` capability is satisfied by it.
    /// Does not consider `preferred` (those are scored, not gated).
    #[must_use]
    pub fn is_satisfied_by(&self, profile: &AgentCapabilityProfile) -> bool {
        self.required.iter().all(|c| c.is_satisfied_by(profile))
            && !self.forbidden.iter().any(|c| c.is_satisfied_by(profile))
    }

    /// Count the `preferred` capabilities satisfied by `profile`. Used
    /// by the router's tie-breaker step 2 (higher score wins).
    #[must_use]
    pub fn preferred_score(&self, profile: &AgentCapabilityProfile) -> usize {
        self.preferred
            .iter()
            .filter(|c| c.is_satisfied_by(profile))
            .count()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reversibility::ReversibilityClass;

    fn opus_profile() -> AgentCapabilityProfile {
        AgentCapabilityProfile {
            agent_id: AgentId::new("claude-opus-4-7-strong").unwrap(),
            display_name: "Claude Opus 4.8".to_string(),
            vendor: VendorClass::Anthropic,
            model_id: "claude-opus-4.8".to_string(),
            declared: vec![
                Capability::Reasoning(ReasoningLevel::Deep),
                Capability::ToolUse(vec![ToolKind::Edit, ToolKind::Write, ToolKind::Bash]),
                Capability::StructuredOutput(SchemaRef::AuditorVerdict),
                Capability::LatencyBudget(30000),
                Capability::CostBudget(0.50),
                Capability::ReversibilityClass(ReversibilityClass::Catastrophic),
            ],
            cost_per_input_token: 0.000_015,
            cost_per_output_token: 0.000_075,
            typical_latency_ms: 6000,
            max_context_tokens: 200_000,
            data_zones: vec![DataZone::UsEast],
        }
    }

    fn kimi_profile() -> AgentCapabilityProfile {
        AgentCapabilityProfile {
            agent_id: AgentId::new("kimi-k2-6-budget").unwrap(),
            display_name: "Kimi K2.6 (budget)".to_string(),
            vendor: VendorClass::Other {
                underlying_vendor: Box::new(VendorClass::Unknown),
            },
            model_id: "kimi-k2.6".to_string(),
            declared: vec![
                Capability::Reasoning(ReasoningLevel::Standard),
                Capability::ToolUse(vec![ToolKind::Edit, ToolKind::Write, ToolKind::Bash]),
                Capability::StructuredOutput(SchemaRef::AuditorVerdict),
                Capability::LatencyBudget(15000),
                Capability::CostBudget(0.05),
                Capability::ReversibilityClass(ReversibilityClass::Irreversible),
            ],
            cost_per_input_token: 0.000_001,
            cost_per_output_token: 0.000_005,
            typical_latency_ms: 8000,
            max_context_tokens: 128_000,
            data_zones: vec![],
        }
    }

    // ---- AgentId ----

    #[test]
    fn agent_id_rejects_empty() {
        assert_eq!(AgentId::new(""), Err(AgentIdError::Empty));
        assert_eq!(AgentId::new("   "), Err(AgentIdError::Empty));
    }

    #[test]
    fn agent_id_trims_whitespace() {
        let id = AgentId::new("  opus  ").unwrap();
        assert_eq!(id.as_str(), "opus");
    }

    #[test]
    fn agent_id_displays_inner_string() {
        let id = AgentId::new("agent-1").unwrap();
        assert_eq!(format!("{id}"), "agent-1");
    }

    // ---- VendorClass ----

    #[test]
    fn vendor_class_other_resolves_to_underlying() {
        let openrouter_claude = VendorClass::Other {
            underlying_vendor: Box::new(VendorClass::Anthropic),
        };
        assert_eq!(openrouter_claude.true_vendor(), &VendorClass::Anthropic);
    }

    #[test]
    fn different_vendor_from_compares_true_vendors() {
        let openrouter_claude = VendorClass::Other {
            underlying_vendor: Box::new(VendorClass::Anthropic),
        };
        assert!(
            !openrouter_claude.is_different_from(&VendorClass::Anthropic),
            "openrouter-fronted Claude must NOT be different from Anthropic"
        );
        assert!(openrouter_claude.is_different_from(&VendorClass::Openai));
    }

    #[test]
    fn unknown_vendor_treated_as_distinct() {
        // Two Unknown vendors are treated as distinct — separate
        // gateway calls aren't presumed to share an underlying vendor.
        assert!(VendorClass::Unknown.is_different_from(&VendorClass::Unknown));
        assert!(VendorClass::Unknown.is_different_from(&VendorClass::Anthropic));
        assert!(VendorClass::Anthropic.is_different_from(&VendorClass::Unknown));
    }

    // ---- ReasoningLevel ordering ----

    #[test]
    fn reasoning_level_ordering() {
        assert!(ReasoningLevel::Deep > ReasoningLevel::Standard);
        assert!(ReasoningLevel::Standard > ReasoningLevel::Shallow);
        assert!(ReasoningLevel::Deep.at_least(ReasoningLevel::Standard));
        assert!(!ReasoningLevel::Shallow.at_least(ReasoningLevel::Deep));
        assert!(ReasoningLevel::Standard.at_least(ReasoningLevel::Standard));
    }

    // ---- Capability satisfaction ----

    #[test]
    fn reasoning_capability_satisfied_when_deeper() {
        let req = Capability::Reasoning(ReasoningLevel::Standard);
        assert!(
            req.is_satisfied_by(&opus_profile()),
            "Deep satisfies Standard"
        );
        assert!(
            req.is_satisfied_by(&kimi_profile()),
            "Standard satisfies Standard"
        );
    }

    #[test]
    fn reasoning_capability_fails_when_shallower() {
        let req = Capability::Reasoning(ReasoningLevel::Deep);
        assert!(req.is_satisfied_by(&opus_profile()), "Deep satisfies Deep");
        assert!(
            !req.is_satisfied_by(&kimi_profile()),
            "Standard does not satisfy Deep"
        );
    }

    #[test]
    fn tool_use_requires_superset() {
        let req = Capability::ToolUse(vec![ToolKind::Edit, ToolKind::Bash]);
        assert!(req.is_satisfied_by(&opus_profile()));
        // Requirement with a tool not declared:
        let req2 = Capability::ToolUse(vec![ToolKind::Edit, ToolKind::WebFetch]);
        assert!(
            !req2.is_satisfied_by(&opus_profile()),
            "WebFetch undeclared"
        );
    }

    #[test]
    fn cost_budget_eliminates_when_actual_exceeds_budget() {
        // Tighter budget — Kimi (0.05) qualifies; Opus (0.50) doesn't.
        let strict = Capability::CostBudget(0.10);
        assert!(strict.is_satisfied_by(&kimi_profile()));
        assert!(!strict.is_satisfied_by(&opus_profile()));
    }

    #[test]
    fn latency_budget_lte_semantics() {
        let budget = Capability::LatencyBudget(20000);
        assert!(
            budget.is_satisfied_by(&kimi_profile()),
            "kimi 15000 satisfies 20000 budget"
        );
        assert!(
            !budget.is_satisfied_by(&opus_profile()),
            "opus 30000 exceeds 20000 budget"
        );
    }

    #[test]
    fn vendor_match() {
        let req = Capability::Vendor(VendorClass::Anthropic);
        assert!(req.is_satisfied_by(&opus_profile()));
        assert!(!req.is_satisfied_by(&kimi_profile()));
    }

    #[test]
    fn different_vendor_from_satisfied_when_distinct() {
        let req = Capability::DifferentVendorFrom(VendorClass::Anthropic);
        assert!(
            !req.is_satisfied_by(&opus_profile()),
            "opus IS Anthropic — fails"
        );
        assert!(
            req.is_satisfied_by(&kimi_profile()),
            "kimi is non-Anthropic — passes"
        );
    }

    #[test]
    fn structured_output_exact_schema_match() {
        let req = Capability::StructuredOutput(SchemaRef::AuditorVerdict);
        assert!(req.is_satisfied_by(&opus_profile()));
        let req2 = Capability::StructuredOutput(SchemaRef::BaCritique);
        assert!(
            !req2.is_satisfied_by(&opus_profile()),
            "opus doesn't declare BaCritique"
        );
    }

    #[test]
    fn structured_output_any_json_satisfied_by_specific_schema() {
        let req = Capability::StructuredOutput(SchemaRef::AnyJson);
        assert!(
            req.is_satisfied_by(&opus_profile()),
            "AnyJson should be satisfied by AuditorVerdict declaration"
        );
    }

    #[test]
    fn open_weights_satisfied_only_when_declared() {
        let mut local_ollama = kimi_profile();
        local_ollama.declared.push(Capability::OpenWeights);
        let req = Capability::OpenWeights;
        assert!(req.is_satisfied_by(&local_ollama));
        assert!(!req.is_satisfied_by(&opus_profile()));
    }

    #[test]
    fn reversibility_class_satisfied_when_qualified_for_higher() {
        // Opus declares Catastrophic — qualified for Irreversible too.
        let req = Capability::ReversibilityClass(ReversibilityClass::Irreversible);
        assert!(req.is_satisfied_by(&opus_profile()));
        // Kimi declares Irreversible — NOT qualified for Catastrophic.
        let req2 = Capability::ReversibilityClass(ReversibilityClass::Catastrophic);
        assert!(!req2.is_satisfied_by(&kimi_profile()));
    }

    // ---- CapabilityRequirement composition ----

    #[test]
    fn requirement_passes_when_all_required_satisfied() {
        let req = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Standard),
            Capability::StructuredOutput(SchemaRef::AuditorVerdict),
        ]);
        assert!(req.is_satisfied_by(&opus_profile()));
        assert!(req.is_satisfied_by(&kimi_profile()));
    }

    #[test]
    fn requirement_fails_when_any_required_unsatisfied() {
        let req = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Deep), // kimi: Standard
        ]);
        assert!(req.is_satisfied_by(&opus_profile()));
        assert!(!req.is_satisfied_by(&kimi_profile()));
    }

    #[test]
    fn requirement_fails_when_forbidden_satisfied() {
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_forbidden(Capability::Vendor(VendorClass::Anthropic));
        assert!(!req.is_satisfied_by(&opus_profile()), "Anthropic forbidden");
        assert!(req.is_satisfied_by(&kimi_profile()));
    }

    #[test]
    fn preferred_score_counts_satisfied_preferred() {
        let req = CapabilityRequirement::new(vec![Capability::Reasoning(ReasoningLevel::Standard)])
            .with_preferred(Capability::Reasoning(ReasoningLevel::Deep))
            .with_preferred(Capability::CostBudget(0.10));
        // opus: Deep satisfies preferred[0]; CostBudget(0.50) does NOT
        // satisfy CostBudget(0.10) (over budget) → score = 1.
        assert_eq!(req.preferred_score(&opus_profile()), 1);
        // kimi: Standard does NOT satisfy Deep preferred; CostBudget
        // 0.05 satisfies 0.10 → score = 1.
        assert_eq!(req.preferred_score(&kimi_profile()), 1);
    }

    #[test]
    fn a3_auditor_requirement_picks_different_vendor() {
        // Realistic A3 use case: acting agent is Anthropic; auditor
        // must come from a different vendor + produce AuditorVerdict.
        let req = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Standard),
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
            Capability::StructuredOutput(SchemaRef::AuditorVerdict),
        ])
        .with_preferred(Capability::Reasoning(ReasoningLevel::Deep));
        assert!(
            !req.is_satisfied_by(&opus_profile()),
            "Anthropic disqualified"
        );
        assert!(req.is_satisfied_by(&kimi_profile()), "Kimi qualifies");
    }

    #[test]
    fn catastrophic_action_routing_demands_deep_reasoning_no_cost_shortcut() {
        // Per spec §5.4: catastrophic actions demand Deep + forbid cost budget shortcuts.
        let req = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Deep),
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
            Capability::StructuredOutput(SchemaRef::AuditorVerdict),
        ])
        .with_forbidden(Capability::CostBudget(0.05));
        // Kimi declares Standard reasoning → required Deep fails.
        assert!(!req.is_satisfied_by(&kimi_profile()));
        // (No deep+different-vendor profile in the fixture set — that's
        // the operator's responsibility to register; spec §7.6 calls
        // out catastrophic-on-A3 needing 3 vendors registered.)
    }

    // ---- Serde round-trip ----

    #[test]
    fn capability_roundtrips_through_json() {
        let original = Capability::Reasoning(ReasoningLevel::Deep);
        let json = serde_json::to_string(&original).unwrap();
        let parsed: Capability = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn requirement_roundtrips_through_json() {
        let original = CapabilityRequirement::new(vec![
            Capability::Reasoning(ReasoningLevel::Standard),
            Capability::DifferentVendorFrom(VendorClass::Anthropic),
        ])
        .with_preferred(Capability::Reasoning(ReasoningLevel::Deep));
        let json = serde_json::to_string(&original).unwrap();
        let parsed: CapabilityRequirement = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn profile_roundtrips_through_json() {
        let original = opus_profile();
        let json = serde_json::to_string(&original).unwrap();
        let parsed: AgentCapabilityProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    // ---- Send + Sync ----

    #[test]
    fn types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Capability>();
        assert_send_sync::<CapabilityRequirement>();
        assert_send_sync::<AgentCapabilityProfile>();
        assert_send_sync::<AgentId>();
    }
}
